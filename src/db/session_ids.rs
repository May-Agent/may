//! The daemon's two sources of model-facing ids: the global generator of
//! session names, and each session's pool of per-session ids.
//!
//! A session name is recorded in `session_names` when it is issued or a
//! session is created under it, and the row is never deleted, so no name is
//! issued twice. A session's pool is built the first time something of the
//! session leases from it, with the words of the ids the database keeps for
//! the session (open questions, scheduled tasks) leased again. Only the
//! daemon builds pools: a database another process opens
//! ([`ChatDb::lease_ids_from`]) leases from the daemon's.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::model_ids::SessionIds;
use crate::session::SessionId;

use super::{ChatDb, ChatDbError, query};

/// Each session's pool, by session id.
#[derive(Debug, Clone, Default)]
pub(super) struct SessionPools {
    pools: Arc<Mutex<HashMap<String, Arc<SessionIds>>>>,
    /// The daemon whose pools this process leases from, when it is not the
    /// daemon itself.
    daemon: Option<crate::client::ids::DaemonIds>,
}

impl SessionPools {
    fn pools(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<SessionIds>>> {
        self.pools.lock().expect("session id pools lock poisoned")
    }

    /// The pool of `session_id` leasing from `daemon`, built on first use.
    fn leased_from(
        &self,
        daemon: &crate::client::ids::DaemonIds,
        session_id: &str,
    ) -> Arc<SessionIds> {
        self.pools()
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(daemon.session(session_id)))
            .clone()
    }
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct NameRow {
    name: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct WordRow {
    word: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct CountRow {
    count: i64,
}

impl ChatDb {
    /// Issues a session name no session was ever named by.
    ///
    /// # Errors
    /// [`ChatDbError::SessionNamesExhausted`] when every name was issued;
    /// storage failures otherwise.
    pub async fn issue_session_name(&self) -> Result<SessionId, ChatDbError> {
        let issued = query!(self.db, "SELECT COUNT(*) AS count FROM session_names")
            .fetch_one::<CountRow>()
            .await?
            .count;
        if u64::try_from(issued)
            .is_ok_and(|issued| issued >= crate::model_ids::session_name_capacity())
        {
            return Err(ChatDbError::SessionNamesExhausted);
        }
        let mut rng = fastrand::Rng::new();
        loop {
            let name = crate::model_ids::draw_session_name(&mut rng);
            if let Some(row) = query!(
                self.db,
                "INSERT INTO session_names (name) VALUES (?) ON CONFLICT(name) DO NOTHING RETURNING name",
                name.as_str()
            )
            .fetch_optional::<NameRow>()
            .await?
            {
                return Ok(SessionId::from_string(row.name));
            }
        }
    }

    /// Whether `name` was issued and no session has been created under it.
    ///
    /// # Errors
    /// Storage failures.
    pub async fn session_name_unused(&self, name: &str) -> Result<bool, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT name FROM session_names WHERE name = ? AND name NOT IN (SELECT id FROM sessions)",
            name
        )
        .fetch_optional::<NameRow>()
        .await?
        .is_some())
    }

    /// The pool of `session_id`'s model-facing ids.
    ///
    /// # Errors
    /// [`ChatDbError::StoredIdUnavailable`] when two ids the database keeps
    /// for the session share a word or one is not a pool word; storage
    /// failures otherwise.
    pub async fn session_ids(&self, session_id: &str) -> Result<Arc<SessionIds>, ChatDbError> {
        if let Some(daemon) = &self.session_pools.daemon {
            return Ok(self.session_pools.leased_from(daemon, session_id));
        }
        if let Some(ids) = self.session_pools.pools().get(session_id) {
            return Ok(ids.clone());
        }
        let stored = query!(
            self.db,
            "SELECT word FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND word IS NOT NULL UNION ALL SELECT word FROM scheduled_tasks WHERE session_id = ? AND word IS NOT NULL",
            session_id,
            session_id
        )
        .fetch_all::<WordRow>()
        .await?;
        let built = Arc::new(SessionIds::restore(stored.into_iter().map(|row| row.word))?);
        // A pool another caller built meanwhile wins; this one's leases go.
        Ok(self
            .session_pools
            .pools()
            .entry(session_id.to_string())
            .or_insert(built)
            .clone())
    }

    /// This database's per-session ids leased from `daemon`'s pools, for a
    /// process that is not the daemon: the daemon owns every session's pool,
    /// so an id this process leases is never one the daemon leased.
    #[must_use]
    pub fn lease_ids_from(mut self, daemon: crate::client::ids::DaemonIds) -> Self {
        self.session_pools = SessionPools {
            pools: Arc::default(),
            daemon: Some(daemon),
        };
        self
    }

    /// Ends the lease of `session_id`'s stored id `word`, once the database
    /// no longer keeps it. A pool of this process not built yet holds no
    /// lease; the daemon's always might.
    pub(super) fn release_session_word(&self, session_id: &str, word: &str) {
        if let Some(daemon) = &self.session_pools.daemon {
            self.session_pools
                .leased_from(daemon, session_id)
                .release(word);
            return;
        }
        let ids = self.session_pools.pools().get(session_id).cloned();
        if let Some(ids) = ids {
            ids.release(word);
        }
    }

    /// Drops the pool of a deleted session, or of every session on `None`.
    pub(super) fn forget_session_ids(&self, session_id: Option<&str>) {
        let mut pools = self.session_pools.pools();
        match session_id {
            Some(session_id) => {
                pools.remove(session_id);
            }
            None => pools.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    async fn open(dir: &std::path::Path) -> ChatDb {
        ChatDb::open(&dir.join("history.db")).await.expect("db")
    }

    #[tokio::test]
    async fn ids_of_every_kind_handed_out_together_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).await;
        let name = db.issue_session_name().await.unwrap();
        db.create_session(name.as_str(), None, None).await.unwrap();
        let question = db
            .open_ask_user_question(
                name.as_str(),
                &crate::session::PersistedAskUserRequest {
                    id: "request".to_string(),
                    questions: Vec::new(),
                },
            )
            .await
            .unwrap();
        // Background jobs, stored outputs, subagents and tunnels lease from
        // the same pool.
        let ids = db.session_ids(name.as_str()).await.unwrap();
        let job = ids.lease().unwrap();
        let tunnel = ids.lease().unwrap();
        let words = [
            name.as_str(),
            question.as_str(),
            job.as_str(),
            tunnel.as_str(),
        ];
        assert_eq!(
            words.iter().collect::<BTreeSet<_>>().len(),
            words.len(),
            "{words:?}"
        );
        assert!(
            ids.allocator().claim(&question).is_err(),
            "the open question holds its word"
        );
    }

    #[tokio::test]
    async fn a_session_name_is_never_issued_twice() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).await;
        let first = db.issue_session_name().await.unwrap();
        assert!(db.session_name_unused(first.as_str()).await.unwrap());
        db.create_session(first.as_str(), None, None).await.unwrap();
        assert!(!db.session_name_unused(first.as_str()).await.unwrap());
        db.delete_session(first.as_str()).await.unwrap();
        // The name stays recorded after its session is gone.
        let recorded = query!(
            db.db,
            "SELECT name FROM session_names WHERE name = ?",
            first.as_str()
        )
        .fetch_optional::<NameRow>()
        .await
        .unwrap();
        assert!(recorded.is_some());
        let names: BTreeSet<String> = {
            let mut names = BTreeSet::new();
            for _ in 0..200 {
                names.insert(db.issue_session_name().await.unwrap().to_string());
            }
            names
        };
        assert_eq!(names.len(), 200);
        assert!(!names.contains(first.as_str()));
    }

    #[tokio::test]
    async fn a_pool_built_after_a_reopen_keeps_the_stored_words_leased() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).await;
        db.create_session("amber-river", None, None).await.unwrap();
        let word = db
            .open_ask_user_question(
                "amber-river",
                &crate::session::PersistedAskUserRequest {
                    id: "request".to_string(),
                    questions: Vec::new(),
                },
            )
            .await
            .unwrap();
        drop(db);

        let db = open(dir.path()).await;
        let ids = db.session_ids("amber-river").await.unwrap();
        assert!(ids.allocator().claim(&word).is_err(), "leased again");
        assert!(
            db.withdraw_ask_user_question("amber-river", "request")
                .await
                .unwrap()
        );
        assert!(ids.allocator().claim(&word).is_ok(), "the word went back");
    }
}
