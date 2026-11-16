//! The stored rows one transcript page is built from.
//!
//! A transcript entry is the row that opens it: every row except a tool
//! event that completes an earlier call (`end`, `stopped`) or does not
//! decode. A page reads the rows of its entries and nothing else, plus the
//! completing tool rows of the calls it opens, which sit after them.

use super::common::{I64ValueRow, storage_error};
use super::entity::messages;
use super::{ChatDb, ChatDbError, query};

/// The rows behind one transcript page.
#[derive(Debug, Clone)]
pub struct TranscriptRows {
    /// The rows that open the page's entries, oldest first.
    pub entries: Vec<messages::Model>,
    /// The `end` and `stopped` tool rows of the calls the page's `start`
    /// rows open, oldest first; they may lie after the page.
    pub completions: Vec<messages::Model>,
    /// Whether entries exist before the page.
    pub has_more: bool,
}

impl ChatDb {
    /// Whether row `row_id` of the session opens a transcript entry.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn is_transcript_entry(
        &self,
        session_id: &str,
        row_id: i64,
    ) -> Result<bool, ChatDbError> {
        // A tool row opens an entry only as a `start` record; `CASE` keeps
        // `json_extract` away from content that is not JSON.
        Ok(query!(
            self.db,
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id = ? AND id = ? AND CASE WHEN role != 'tool' THEN 1 WHEN json_valid(content) THEN json_extract(content, '$.stage') = 'start' ELSE 0 END) AS value",
            session_id,
            row_id
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value
            != 0)
    }

    /// The rows of the `limit` newest transcript entries before row
    /// `before` (all of them without `limit`, the newest ones without
    /// `before`), with the rows that complete the calls they open.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn transcript_rows(
        &self,
        session_id: &str,
        before: Option<i64>,
        limit: Option<u32>,
    ) -> Result<TranscriptRows, ChatDbError> {
        let before = before.unwrap_or(i64::MAX);
        let limit = limit.map_or(i64::MAX, i64::from);
        let mut entries = query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? AND id < ? AND CASE WHEN role != 'tool' THEN 1 WHEN json_valid(content) THEN json_extract(content, '$.stage') = 'start' ELSE 0 END ORDER BY id DESC LIMIT ?",
            session_id,
            before,
            limit
        )
        .fetch_all::<messages::Model>()
        .await?;
        entries.reverse();

        let Some(first) = entries.first() else {
            let has_more = self.has_entry_before(session_id, before).await?;
            return Ok(TranscriptRows {
                entries,
                completions: Vec::new(),
                has_more,
            });
        };
        let first_id = i64::from(first.id);
        let call_ids = entries
            .iter()
            .filter(|row| row.role == "tool")
            .filter_map(|row| row.tool_call_id.as_deref())
            .collect::<Vec<_>>();
        let completions = if call_ids.is_empty() {
            Vec::new()
        } else {
            let call_ids = serde_json::to_string(&call_ids)
                .map_err(|error| storage_error(format!("failed to encode call ids: {error}")))?;
            query!(
                self.db,
                "SELECT * FROM messages WHERE session_id = ? AND id > ? AND role = 'tool' AND tool_call_id IN (SELECT value FROM json_each(?)) AND CASE WHEN json_valid(content) THEN json_extract(content, '$.stage') != 'start' ELSE 0 END ORDER BY id ASC",
                session_id,
                first_id,
                call_ids
            )
            .fetch_all::<messages::Model>()
            .await?
        };
        let has_more = self.has_entry_before(session_id, first_id).await?;
        Ok(TranscriptRows {
            entries,
            completions,
            has_more,
        })
    }

    async fn has_entry_before(&self, session_id: &str, id: i64) -> Result<bool, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id = ? AND id < ? AND CASE WHEN role != 'tool' THEN 1 WHEN json_valid(content) THEN json_extract(content, '$.stage') = 'start' ELSE 0 END) AS value",
            session_id,
            id
        )
        .fetch_one::<I64ValueRow>()
        .await?
        .value
            != 0)
    }
}
