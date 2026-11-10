//! Message scans behind `find` and ranged `session messages`.
//!
//! The filters run in SQL against the `messages` and `sessions` indexes; the
//! caller applies its pattern to the rows a scan returns. Only the text
//! columns are selected: `thinking` and `reasoning` never leave the table.

use super::entity::messages;
use super::{ChatDb, ChatDbError, query};

/// Filters a [`ChatDb::scan_messages`] call narrows by before any pattern runs.
#[derive(Debug, Clone, Default)]
pub struct MessageScan {
    /// Only this session.
    pub session_id: Option<String>,
    /// Message roles that may match.
    pub roles: Vec<String>,
    /// Timestamp lower bound (inclusive, string order).
    pub since: Option<String>,
    /// Timestamp upper bound (inclusive, string order).
    pub until: Option<String>,
    /// Case-insensitive text the session title contains.
    pub title_contains: Option<String>,
}

/// A scanned message with its session's title.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct ScannedMessage {
    /// Message id.
    pub id: i64,
    /// Owning session.
    pub session_id: String,
    /// Session title.
    pub title: Option<String>,
    /// Message role.
    pub role: String,
    /// Message body.
    pub content: String,
    /// Message timestamp, when recorded.
    pub timestamp: Option<String>,
}

impl ChatDb {
    /// Up to `limit` messages passing `scan`'s filters with an id below
    /// `before_id`, newest first.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn scan_messages(
        &self,
        scan: &MessageScan,
        before_id: i64,
        limit: i64,
    ) -> Result<Vec<ScannedMessage>, ChatDbError> {
        let roles = serde_json::to_string(&scan.roles).map_err(|error| {
            ChatDbError::Storage(super::db_custom_error("encode message roles: ", &error))
        })?;
        let session_id = scan.session_id.as_deref();
        let since = scan.since.as_deref();
        let until = scan.until.as_deref();
        let title = scan.title_contains.as_deref();
        Ok(query!(
            self.db,
            "SELECT m.id AS id, m.session_id AS session_id, s.title AS title, m.role AS role, \
             m.content AS content, m.timestamp AS timestamp \
             FROM messages m JOIN sessions s ON s.id = m.session_id \
             WHERE m.id < ? \
             AND (? IS NULL OR m.session_id = ?) \
             AND m.role IN (SELECT value FROM json_each(?)) \
             AND (? IS NULL OR m.timestamp >= ?) \
             AND (? IS NULL OR m.timestamp <= ?) \
             AND (? IS NULL OR instr(lower(COALESCE(s.title, '')), lower(?)) > 0) \
             ORDER BY m.id DESC LIMIT ?",
            before_id,
            session_id,
            session_id,
            roles,
            since,
            since,
            until,
            until,
            title,
            title,
            limit
        )
        .fetch_all::<ScannedMessage>()
        .await?)
    }

    /// The `count` messages of `session_id` just before `message_id` and the
    /// `count` just after it, with the message itself, in id order. `None`
    /// when the message is not in the session.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn messages_around(
        &self,
        session_id: &str,
        message_id: i64,
        count: i64,
    ) -> Result<Option<Vec<messages::Model>>, ChatDbError> {
        let centre = query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? AND id = ?",
            session_id,
            message_id
        )
        .fetch_optional::<messages::Model>()
        .await?;
        let Some(centre) = centre else {
            return Ok(None);
        };
        let mut before = query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? AND id < ? ORDER BY id DESC LIMIT ?",
            session_id,
            message_id,
            count
        )
        .fetch_all::<messages::Model>()
        .await?;
        before.reverse();
        let after = query!(
            self.db,
            "SELECT * FROM messages WHERE session_id = ? AND id > ? ORDER BY id ASC LIMIT ?",
            session_id,
            message_id,
            count
        )
        .fetch_all::<messages::Model>()
        .await?;
        before.push(centre);
        before.extend(after);
        Ok(Some(before))
    }
}
