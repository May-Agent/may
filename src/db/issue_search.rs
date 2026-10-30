//! Local record of the issues pushed into each issue search collection.

use super::{ChatDb, ChatDbError, query};

/// One issue as the search index last saw it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct IssueSearchDoc {
    /// Issue number.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue state (`open` / `closed`).
    pub state: String,
    /// Update timestamp the indexed document reflects.
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct WatermarkRow {
    value: Option<String>,
}

impl ChatDb {
    /// The newest update timestamp indexed into `collection`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn issue_search_watermark(
        &self,
        collection: &str,
    ) -> Result<Option<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT MAX(updated_at) AS value FROM issue_search_docs WHERE collection = ?",
            collection
        )
        .fetch_one::<WatermarkRow>()
        .await?
        .value)
    }

    /// Records that `doc` is indexed in `collection`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the upsert fails.
    pub async fn record_issue_search_doc(
        &self,
        collection: &str,
        doc: &IssueSearchDoc,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO issue_search_docs (collection, number, title, state, updated_at) VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(collection, number) DO UPDATE SET title = excluded.title, state = excluded.state, updated_at = excluded.updated_at",
            collection,
            doc.number,
            doc.title.as_str(),
            doc.state.as_str(),
            doc.updated_at.as_str()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The indexed record of issue `number` in `collection`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn issue_search_doc(
        &self,
        collection: &str,
        number: i64,
    ) -> Result<Option<IssueSearchDoc>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT number, title, state, updated_at FROM issue_search_docs WHERE collection = ? AND number = ?",
            collection,
            number
        )
        .fetch_optional::<IssueSearchDoc>()
        .await?)
    }
}
