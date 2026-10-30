//! Outbox of index changes for repo-search collections.
//!
//! SQLite triggers record every insert and delete of an indexed message, so
//! no writer has to remember the index. The indexer drains the outbox into
//! repo-search and clears what it pushed.

use skyzen_services::Db as SkyDb;

use super::{
    ChatDb, ChatDbError, SqliteTableInfoRow,
    db_sql::{self, SEARCH_OUTBOX_BACKFILL, SEARCH_OUTBOX_SCHEMA},
    query,
};

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct OutboxRow {
    id: i64,
    collection: String,
    doc_id: String,
    message_id: i64,
}

/// One pending index change: the message document to refresh from its row,
/// or to delete when the row is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutboxEntry {
    /// Outbox row id; entries are drained in id order.
    pub id: i64,
    /// Collection the document lives in.
    pub collection: String,
    /// Document id inside the collection.
    pub doc_id: String,
    /// Id of the message the document holds.
    pub message_id: i64,
}

/// A message as the session index stores and filters it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct IndexedMessageRow {
    /// Message id.
    pub message_id: i64,
    /// Owning session.
    pub session_id: String,
    /// Message role.
    pub role: String,
    /// Message body.
    pub content: String,
    /// Message timestamp, when recorded.
    pub timestamp: Option<String>,
    /// Session title.
    pub title: Option<String>,
    /// Session model.
    pub model: Option<String>,
    /// Session profile.
    pub profile_id: Option<String>,
    /// Whether the session is archived.
    pub archived: bool,
    /// Session preview.
    pub preview: Option<String>,
    /// Session update time.
    pub updated_at: String,
    /// Session message count.
    pub message_count: i32,
}

impl ChatDb {
    /// Creates the outbox and its triggers. On the first run it also queues
    /// every existing message, so the collection starts complete.
    pub(super) async fn ensure_search_outbox(db: &SkyDb) -> Result<(), ChatDbError> {
        let existed = !query!(db, "PRAGMA table_info(search_outbox)")
            .fetch_all::<SqliteTableInfoRow>()
            .await?
            .is_empty();
        for statement in db_sql::blocks(SEARCH_OUTBOX_SCHEMA) {
            query!(db, statement).execute().await?;
        }
        if !existed {
            query!(db, SEARCH_OUTBOX_BACKFILL).execute().await?;
        }
        Ok(())
    }

    /// The oldest `limit` pending index changes.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn pending_search_outbox(
        &self,
        limit: i64,
    ) -> Result<Vec<SearchOutboxEntry>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT id, collection, doc_id, message_id FROM search_outbox ORDER BY id LIMIT ?",
            limit
        )
        .fetch_all::<OutboxRow>()
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| SearchOutboxEntry {
                id: row.id,
                collection: row.collection,
                doc_id: row.doc_id,
                message_id: row.message_id,
            })
            .collect())
    }

    /// Number of pending index changes.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn search_outbox_len(&self) -> Result<i64, ChatDbError> {
        Ok(
            query!(self.db, "SELECT COUNT(*) AS value FROM search_outbox")
                .fetch_one::<super::I64ValueRow>()
                .await?
                .value,
        )
    }

    /// Clears every outbox entry up to and including `through_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the delete fails.
    pub async fn clear_search_outbox(&self, through_id: i64) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM search_outbox WHERE id <= ?",
            through_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Messages with `ids` joined with their sessions; ids without a row are
    /// left out.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn indexed_messages(
        &self,
        ids: &[i64],
    ) -> Result<Vec<IndexedMessageRow>, ChatDbError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = serde_json::to_string(ids).map_err(|error| {
            ChatDbError::Storage(super::db_custom_error("encode message ids: ", &error))
        })?;
        Ok(query!(
            self.db,
            "SELECT m.id AS message_id, m.session_id, m.role, m.content, m.timestamp, \
             s.title, s.model, s.profile_id, s.archived, s.preview, s.updated_at, s.message_count \
             FROM messages m JOIN sessions s ON s.id = m.session_id \
             WHERE m.id IN (SELECT value FROM json_each(?))",
            ids
        )
        .fetch_all::<IndexedMessageRow>()
        .await?)
    }
}
