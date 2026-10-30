//! Document collections: May's session and issue indexes inside repo-search.

use std::future::Future;

use repo_search_client::protocol::{CollectionSearch, Document, Format, WireMode};
use serde::Deserialize;

use super::{RepoSearch, RepoSearchError};

/// Token budget for one collection query's answer.
const QUERY_BUDGET: u32 = 16_000;

/// One document a collection query returned, best first.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectionHit {
    /// Document id as it was upserted.
    pub id: String,
    /// Relevance score; higher is better.
    pub score: f64,
    /// Text of the best matching chunk.
    pub excerpt: Option<String>,
}

/// The collection operations May's indexes use. Implemented by the
/// repo-search daemon and, in tests, by an in-memory double.
pub trait CollectionBackend: Send + Sync {
    /// Inserts or replaces `documents` in `collection`, creating it on first use.
    fn upsert(
        &self,
        collection: &str,
        documents: Vec<Document>,
    ) -> impl Future<Output = Result<(), RepoSearchError>> + Send;

    /// Removes the documents with `ids` from `collection`; unknown ids are ignored.
    fn delete(
        &self,
        collection: &str,
        ids: Vec<String>,
    ) -> impl Future<Output = Result<(), RepoSearchError>> + Send;

    /// Natural-language query over `collection`. A collection that was never
    /// written to has no hits.
    fn search(
        &self,
        collection: &str,
        query: &str,
    ) -> impl Future<Output = Result<Vec<CollectionHit>, RepoSearchError>> + Send;
}

/// [`CollectionBackend`] served by the repo-search daemon.
#[derive(Debug, Clone)]
pub struct RepoSearchCollections {
    repo_search: RepoSearch,
}

impl RepoSearchCollections {
    /// Collections reached through `repo_search`.
    #[must_use]
    pub const fn new(repo_search: RepoSearch) -> Self {
        Self { repo_search }
    }
}

impl CollectionBackend for RepoSearchCollections {
    async fn upsert(
        &self,
        collection: &str,
        documents: Vec<Document>,
    ) -> Result<(), RepoSearchError> {
        if documents.is_empty() {
            return Ok(());
        }
        self.repo_search
            .client()
            .await?
            .upsert_documents(collection, documents)
            .await?;
        Ok(())
    }

    async fn delete(&self, collection: &str, ids: Vec<String>) -> Result<(), RepoSearchError> {
        if ids.is_empty() {
            return Ok(());
        }
        match self
            .repo_search
            .client()
            .await?
            .delete_documents(collection, ids)
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if is_missing_collection(&error) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn search(
        &self,
        collection: &str,
        query: &str,
    ) -> Result<Vec<CollectionHit>, RepoSearchError> {
        let answer = match self
            .repo_search
            .client()
            .await?
            .search_collection(CollectionSearch {
                collection: collection.to_string(),
                query: query.to_string(),
                mode: WireMode::Hybrid,
                budget: QUERY_BUDGET,
                ids_only: false,
                format: Format::Json,
            })
            .await
        {
            Ok(answer) => answer,
            Err(error) if is_missing_collection(&error) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        parse_hits(&answer.output)
    }
}

fn is_missing_collection(error: &repo_search_client::client::ClientError) -> bool {
    matches!(
        error,
        repo_search_client::client::ClientError::Refused(report)
            if report.kind == repo_search_client::protocol::ErrorKind::NoSuchCollection
    )
}

#[derive(Debug, Deserialize)]
struct WireResults {
    #[serde(default)]
    docs: Vec<WireDoc>,
}

#[derive(Debug, Deserialize)]
struct WireDoc {
    path: String,
    score: f64,
    #[serde(default)]
    chunks: Vec<WireChunk>,
}

#[derive(Debug, Deserialize)]
struct WireChunk {
    #[serde(default)]
    text: Option<String>,
}

/// Parses repo-search's JSON answer; each doc's `path` is the document id.
fn parse_hits(output: &str) -> Result<Vec<CollectionHit>, RepoSearchError> {
    let results: WireResults = serde_json::from_str(output).map_err(|error| {
        RepoSearchError::Client(repo_search_client::client::ClientError::Frame(error))
    })?;
    Ok(results
        .docs
        .into_iter()
        .map(|doc| CollectionHit {
            id: doc.path,
            score: doc.score,
            excerpt: doc.chunks.into_iter().find_map(|chunk| chunk.text),
        })
        .collect())
}

#[cfg(test)]
pub mod testing {
    //! In-memory [`CollectionBackend`] that ranks by shared query words.

    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use repo_search_client::protocol::Document;

    use super::{CollectionBackend, CollectionHit, RepoSearchError};

    /// Collections held in memory; clones share state.
    #[derive(Debug, Clone, Default)]
    pub struct MemoryCollections {
        collections: Arc<Mutex<BTreeMap<String, BTreeMap<String, Document>>>>,
    }

    impl MemoryCollections {
        /// Ids stored in `collection`, sorted.
        #[must_use]
        pub fn ids(&self, collection: &str) -> Vec<String> {
            self.collections
                .lock()
                .expect("collections lock")
                .get(collection)
                .map(|docs| docs.keys().cloned().collect())
                .unwrap_or_default()
        }
    }

    impl CollectionBackend for MemoryCollections {
        async fn upsert(
            &self,
            collection: &str,
            documents: Vec<Document>,
        ) -> Result<(), RepoSearchError> {
            let mut collections = self.collections.lock().expect("collections lock");
            let docs = collections.entry(collection.to_string()).or_default();
            for document in documents {
                docs.insert(document.id.clone(), document);
            }
            Ok(())
        }

        async fn delete(&self, collection: &str, ids: Vec<String>) -> Result<(), RepoSearchError> {
            if let Some(docs) = self
                .collections
                .lock()
                .expect("collections lock")
                .get_mut(collection)
            {
                for id in ids {
                    docs.remove(&id);
                }
            }
            Ok(())
        }

        async fn search(
            &self,
            collection: &str,
            query: &str,
        ) -> Result<Vec<CollectionHit>, RepoSearchError> {
            let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            let collections = self.collections.lock().expect("collections lock");
            let Some(docs) = collections.get(collection) else {
                return Ok(Vec::new());
            };
            let mut hits: Vec<CollectionHit> = docs
                .values()
                .filter_map(|doc| {
                    let haystack = format!("{} {}", doc.title, doc.text).to_lowercase();
                    let score = words.iter().filter(|word| haystack.contains(*word)).count();
                    (score > 0).then(|| CollectionHit {
                        id: doc.id.clone(),
                        score: score as f64,
                        excerpt: Some(doc.text.clone()),
                    })
                })
                .collect();
            hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
            Ok(hits)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hits;

    #[test]
    fn hits_are_read_from_the_json_answer() {
        let output = r#"{"docs":[{"path":"s1:4","score":0.9,"chunks":[{"lines":[1,3],"heading":"user","channels":[],"text":"reset the password"}]},{"path":"s2:7","score":0.4}],"freshness":{}}"#;
        let hits = parse_hits(output).expect("valid answer");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, "s1:4");
        assert_eq!(hits[0].excerpt.as_deref(), Some("reset the password"));
        assert_eq!(hits[1].excerpt, None);
    }
}
