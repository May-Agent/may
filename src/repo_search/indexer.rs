//! Pushes the search outbox into repo-search collections.

use std::collections::BTreeMap;
use std::time::Duration;

use askama::Template;
use repo_search_client::protocol::Document;

use super::{CollectionBackend, RepoSearchError};
use crate::db::{ChatDb, ChatDbError, SearchOutboxEntry};

/// Collection holding one document per indexed session message.
pub const SESSIONS_COLLECTION: &str = "may-sessions";

/// Outbox entries pushed per round trip.
const BATCH: i64 = 64;

/// Idle wait between drains when nothing wakes the indexer.
pub const DRAIN_INTERVAL: Duration = Duration::from_secs(30);

/// Why a drain stopped early.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Reading the outbox or its source rows failed.
    #[error(transparent)]
    Database(#[from] ChatDbError),
    /// repo-search refused or could not be reached.
    #[error(transparent)]
    RepoSearch(#[from] RepoSearchError),
    /// A document failed to render.
    #[error("failed to render a search document: {0}")]
    Render(#[from] askama::Error),
}

#[derive(Template)]
#[template(path = "search_message_title.txt")]
struct MessageTitle<'a> {
    role: &'a str,
    timestamp: Option<&'a str>,
}

/// Session id and message id of a session-collection document id.
#[must_use]
pub fn parse_message_doc_id(doc_id: &str) -> Option<(&str, i64)> {
    let (session_id, message_id) = doc_id.rsplit_once(':')?;
    Some((session_id, message_id.parse().ok()?))
}

/// The documents of one outbox batch: the latest state of every touched
/// message, or a delete for messages that are gone.
#[derive(Debug, Default)]
struct Changes {
    upserts: BTreeMap<String, Vec<Document>>,
    deletes: BTreeMap<String, Vec<String>>,
}

/// Drains the search outbox into a [`CollectionBackend`].
#[derive(Debug, Clone)]
pub struct SearchIndexer<B> {
    db: ChatDb,
    backend: B,
}

impl<B: CollectionBackend> SearchIndexer<B> {
    /// Indexer pushing `db`'s outbox into `backend`.
    #[must_use]
    pub const fn new(db: ChatDb, backend: B) -> Self {
        Self { db, backend }
    }

    /// The database whose outbox is drained.
    #[must_use]
    pub const fn db(&self) -> &ChatDb {
        &self.db
    }

    /// The backend documents are pushed to.
    #[must_use]
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Pushes every pending change, oldest first, and returns how many
    /// entries were cleared. Entries are cleared only after repo-search
    /// accepted them, so a failed drain retries them next time.
    ///
    /// # Errors
    /// Stops at the first failed batch.
    pub async fn drain(&self) -> Result<usize, IndexerError> {
        let mut cleared = 0;
        loop {
            let entries = self.db.pending_search_outbox(BATCH).await?;
            let Some(last) = entries.last().map(|entry| entry.id) else {
                return Ok(cleared);
            };
            let changes = self.changes(&entries).await?;
            for (collection, ids) in changes.deletes {
                self.backend.delete(&collection, ids).await?;
            }
            for (collection, documents) in changes.upserts {
                self.backend.upsert(&collection, documents).await?;
            }
            self.db.clear_search_outbox(last).await?;
            cleared += entries.len();
        }
    }

    async fn changes(&self, entries: &[SearchOutboxEntry]) -> Result<Changes, IndexerError> {
        // The last entry per document wins; its message row says what is current.
        let mut latest: BTreeMap<(&str, &str), &SearchOutboxEntry> = BTreeMap::new();
        for entry in entries {
            latest.insert((entry.collection.as_str(), entry.doc_id.as_str()), entry);
        }
        let message_ids: Vec<i64> = latest.values().map(|entry| entry.message_id).collect();
        let messages: BTreeMap<i64, _> = self
            .db
            .indexed_messages(&message_ids)
            .await?
            .into_iter()
            .map(|row| (row.message_id, row))
            .collect();

        let mut changes = Changes::default();
        for entry in latest.into_values() {
            let document = match messages.get(&entry.message_id) {
                Some(row) if !row.content.trim().is_empty() => Some(Document {
                    id: entry.doc_id.clone(),
                    title: MessageTitle {
                        role: &row.role,
                        timestamp: row.timestamp.as_deref(),
                    }
                    .render()?,
                    text: row.content.clone(),
                }),
                _ => None,
            };
            match document {
                Some(document) => changes
                    .upserts
                    .entry(entry.collection.clone())
                    .or_default()
                    .push(document),
                None => changes
                    .deletes
                    .entry(entry.collection.clone())
                    .or_default()
                    .push(entry.doc_id.clone()),
            }
        }
        Ok(changes)
    }

    /// Drains whenever `wake` fires and every [`DRAIN_INTERVAL`] otherwise,
    /// backing off while repo-search is unavailable. Never returns.
    pub async fn run(self, wake: async_channel::Receiver<()>, status: IndexerStatus) {
        let mut backoff = DRAIN_INTERVAL;
        loop {
            match self.drain().await {
                Ok(_) => {
                    backoff = DRAIN_INTERVAL;
                    status.set(None, self.db.search_outbox_len().await.unwrap_or_default());
                }
                Err(error) => {
                    tracing::debug!(%error, "search outbox drain failed; retrying later");
                    status.set(
                        Some(error.to_string()),
                        self.db.search_outbox_len().await.unwrap_or_default(),
                    );
                    backoff = (backoff * 2).min(Duration::from_mins(15));
                }
            }
            let timer = async {
                async_io::Timer::after(backoff).await;
            };
            let woken = async {
                let _ = wake.recv().await;
            };
            futures_lite::future::or(timer, woken).await;
        }
    }
}

/// The indexer's latest outcome, read by the daemon's status surfaces.
#[derive(Debug, Clone, Default)]
pub struct IndexerStatus {
    inner: std::sync::Arc<std::sync::Mutex<IndexerSnapshot>>,
}

/// Point-in-time indexer state.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct IndexerSnapshot {
    /// Outbox entries not yet in repo-search.
    pub pending: i64,
    /// Why the latest drain failed, if it did.
    pub last_error: Option<String>,
}

impl IndexerStatus {
    fn set(&self, last_error: Option<String>, pending: i64) {
        if let Ok(mut snapshot) = self.inner.lock() {
            *snapshot = IndexerSnapshot {
                pending,
                last_error,
            };
        }
    }

    /// Current state.
    #[must_use]
    pub fn snapshot(&self) -> IndexerSnapshot {
        self.inner
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{SESSIONS_COLLECTION, SearchIndexer, parse_message_doc_id};
    use crate::db::{ChatDb, MessageMeta};
    use crate::repo_search::CollectionBackend;
    use crate::repo_search::backend::testing::MemoryCollections;
    use crate::session::MessageRole;

    async fn db() -> (tempfile::TempDir, ChatDb) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open db");
        (dir, db)
    }

    #[test]
    fn doc_ids_split_at_the_last_colon() {
        assert_eq!(parse_message_doc_id("abc:12"), Some(("abc", 12)));
        assert_eq!(parse_message_doc_id("abc"), None);
    }

    #[tokio::test]
    async fn messages_reach_the_collection_and_leave_it_on_delete() {
        let (_dir, db) = db().await;
        let session = db
            .create_session("s1", Some("Plan"), Some("m"))
            .await
            .expect("session");
        db.append_message(
            &session.id,
            MessageRole::User,
            "how do we rotate the signing key",
            None,
            MessageMeta::default(),
        )
        .await
        .expect("append");
        db.append_message(
            &session.id,
            MessageRole::HiddenContext,
            "hidden",
            None,
            MessageMeta::default(),
        )
        .await
        .expect("append hidden");
        let backend = MemoryCollections::default();
        let indexer = SearchIndexer::new(db.clone(), backend.clone());

        assert_eq!(indexer.drain().await.expect("drain"), 1);
        let ids = backend.ids(SESSIONS_COLLECTION);
        assert_eq!(ids.len(), 1);
        let hits = backend
            .search(SESSIONS_COLLECTION, "signing key")
            .await
            .expect("search");
        assert_eq!(hits[0].id, ids[0]);

        db.delete_session(&session.id).await.expect("delete");
        indexer.drain().await.expect("drain deletes");
        assert_eq!(
            backend.ids(SESSIONS_COLLECTION),
            [] as [std::string::String; 0]
        );
        assert_eq!(db.search_outbox_len().await.expect("len"), 0);
    }
}
