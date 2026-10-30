//! `issue search`: natural-language search over a backend's issues.
//!
//! Each issue backend has its own repo-search document collection. Before a
//! query, the issues updated since the newest indexed one are pulled from
//! the backend and upserted, so the index follows both may and GitHub
//! issues without hooks on either.

use askama::Template;
use repo_search_client::protocol::Document;

use super::{IssueDetail, IssueError, IssueListState, IssueStore};
use crate::db::{ChatDb, IssueSearchDoc};
use crate::repo_search::CollectionBackend;

/// Issues pulled from the backend per sync page.
const SYNC_PAGE: i64 = 100;
/// Pages pulled per query; a large backlog finishes over later queries.
const SYNC_PAGES_PER_QUERY: usize = 5;

/// Collection of the issues of the backend named `search_key`.
#[must_use]
pub fn collection_name(search_key: &str) -> String {
    let mut name = String::from("may-issues:");
    name.push_str(search_key);
    name
}

#[derive(Template)]
#[template(path = "issue_search_document.txt", escape = "none")]
struct IssueDocumentText<'a> {
    detail: &'a IssueDetail,
}

/// One issue a search returned, best first.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueHit {
    /// Issue number.
    pub number: i64,
    /// Issue title.
    pub title: String,
    /// Issue state.
    pub state: String,
    /// Best matching passage.
    pub excerpt: Option<String>,
}

/// Issue search over a [`CollectionBackend`].
#[derive(Debug, Clone)]
pub struct IssueSearch<B> {
    db: ChatDb,
    backend: B,
}

impl<B: CollectionBackend> IssueSearch<B> {
    /// Search whose index lives in `backend`, with its sync record in `db`.
    #[must_use]
    pub const fn new(db: ChatDb, backend: B) -> Self {
        Self { db, backend }
    }

    /// Pulls issues updated since the last sync from `store` into the index.
    ///
    /// # Errors
    /// Fails when the backend, repo-search or the database fails.
    pub async fn sync<S: IssueStore>(&self, store: &S) -> Result<(), IssueError> {
        let collection = collection_name(&store.search_key());
        for _ in 0..SYNC_PAGES_PER_QUERY {
            let since = self.db.issue_search_watermark(&collection).await?;
            let updated = store.updated_since(since.as_deref(), SYNC_PAGE).await?;
            if updated.is_empty() {
                return Ok(());
            }
            let mut documents = Vec::with_capacity(updated.len());
            let mut records = Vec::with_capacity(updated.len());
            for summary in &updated {
                let detail = store.get(summary.number).await?;
                documents.push(Document {
                    id: detail.number.to_string(),
                    title: detail.title.clone(),
                    text: IssueDocumentText { detail: &detail }
                        .render()
                        .map_err(|error| IssueError::Search(error.to_string()))?,
                });
                records.push(IssueSearchDoc {
                    number: detail.number,
                    title: detail.title.clone(),
                    state: detail.state.as_str().to_string(),
                    updated_at: summary.updated_at.clone(),
                });
            }
            self.backend
                .upsert(&collection, documents)
                .await
                .map_err(|error| IssueError::Search(error.to_string()))?;
            for record in &records {
                self.db.record_issue_search_doc(&collection, record).await?;
            }
            if i64::try_from(updated.len()).unwrap_or(i64::MAX) < SYNC_PAGE {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Syncs, then returns the issues best matching `query` in `state`.
    ///
    /// # Errors
    /// Fails when the sync or the query fails.
    pub async fn search<S: IssueStore>(
        &self,
        store: &S,
        query: &str,
        state: IssueListState,
        limit: usize,
    ) -> Result<Vec<IssueHit>, IssueError> {
        self.sync(store).await?;
        let collection = collection_name(&store.search_key());
        let hits = self
            .backend
            .search(&collection, query)
            .await
            .map_err(|error| IssueError::Search(error.to_string()))?;
        let mut results = Vec::new();
        for hit in hits {
            let Ok(number) = hit.id.parse::<i64>() else {
                continue;
            };
            let Some(doc) = self.db.issue_search_doc(&collection, number).await? else {
                continue;
            };
            let wanted = match state {
                IssueListState::All => true,
                IssueListState::Open | IssueListState::Closed => doc.state == state.as_str(),
            };
            if wanted {
                results.push(IssueHit {
                    number,
                    title: doc.title,
                    state: doc.state,
                    excerpt: hit.excerpt,
                });
            }
            if results.len() >= limit {
                break;
            }
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::IssueSearch;
    use crate::db::ChatDb;
    use crate::issue::{IssueListState, IssueState, IssueStore, MayIssueStore};
    use crate::repo_search::backend::testing::MemoryCollections;

    #[tokio::test]
    async fn issues_are_synced_and_filtered_by_state() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open db");
        let store = MayIssueStore::new(db.clone(), "repo".to_string());
        store
            .create(
                "Login token expires early",
                "Refresh happens too late.",
                &[],
            )
            .await
            .expect("create");
        let closed = store
            .create("Token refresh flakes in CI", "", &[])
            .await
            .expect("create");
        store
            .edit(closed.number, None, None, Some(IssueState::Closed))
            .await
            .expect("close");
        let search = IssueSearch::new(db, MemoryCollections::default());

        let open = search
            .search(&store, "token refresh", IssueListState::Open, 10)
            .await
            .expect("search");
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].number, 1);

        store
            .comment(1, "Also reproduces with the refresh token flow")
            .await
            .expect("comment");
        let all = search
            .search(&store, "token refresh", IssueListState::All, 10)
            .await
            .expect("search");
        assert_eq!(all.len(), 2);
    }
}
