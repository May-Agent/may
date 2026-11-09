//! The model-facing aliases the database keeps, and the namespace they and
//! every other model-facing id are leased from.
//!
//! An alias row names a thing of one [`AliasKind`] by its internal id
//! (`target`) and holds its word. While the row is live its word is leased,
//! and it is leased again when the database opens, so the word names the same
//! thing across daemon restarts. A row is retired once its thing can no
//! longer be addressed — a session archived or deleted, a question answered
//! or withdrawn, a scheduled task deleted: its word goes back to the
//! namespace, and the row stays, so a word the model wrote earlier still
//! tells which thing it named.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aither_core::{AnyIdAllocator, IdLease};
use skyzen_services::Db as SkyDb;

use crate::model_ids::{AgentAlias, AliasKind, QuestionAlias, TaskAlias};

use super::{ChatDb, ChatDbError, db_sql, query};

/// The leases of the live alias rows, by kind and target.
type Held = HashMap<(&'static str, String), IdLease>;

/// The namespace of a database: its allocator, and the leases of its live
/// alias rows.
#[derive(Clone)]
pub(super) struct ModelNamespace {
    ids: AnyIdAllocator,
    held: Arc<Mutex<Held>>,
}

impl std::fmt::Debug for ModelNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelNamespace").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct AliasRow {
    kind: String,
    target: String,
    word: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct KindTargetRow {
    kind: String,
    target: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct WordRow {
    word: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct TargetRow {
    target: String,
}

/// Retires the rows whose thing ended, returning their kinds and targets.
async fn retire_ended(db: &SkyDb) -> Result<Vec<KindTargetRow>, ChatDbError> {
    let mut retired = Vec::new();
    for statement in db_sql::statements(db_sql::MODEL_ALIASES_RETIRE_ENDED) {
        retired.extend(query!(db, statement).fetch_all::<KindTargetRow>().await?);
    }
    Ok(retired)
}

/// The static kind name a stored row carries.
fn kind_named(kind: &str) -> &'static str {
    [AgentAlias::KIND, QuestionAlias::KIND, TaskAlias::KIND]
        .into_iter()
        .find(|known| *known == kind)
        .unwrap_or_else(|| panic!("model_aliases holds the unknown kind '{kind}'"))
}

impl ModelNamespace {
    /// The namespace of `db`: rows whose thing ended are retired, and the
    /// word of every live row is leased again.
    pub(super) async fn restore(db: &SkyDb) -> Result<Self, ChatDbError> {
        retire_ended(db).await?;
        let ids = crate::model_ids::allocator();
        let mut held = Held::new();
        for row in query!(
            db,
            "SELECT kind, target, word FROM model_aliases WHERE live = 1"
        )
        .fetch_all::<AliasRow>()
        .await?
        {
            held.insert((kind_named(&row.kind), row.target), ids.claim(&row.word)?);
        }
        Ok(Self {
            ids,
            held: Arc::new(Mutex::new(held)),
        })
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().expect("model alias leases lock poisoned")
    }
}

impl ChatDb {
    /// The allocator every model-facing id of this database's daemon is
    /// leased from: terminals, subagents and tunnels lease theirs here, as
    /// the aliases do.
    #[must_use]
    pub const fn model_ids(&self) -> &AnyIdAllocator {
        &self.namespace.ids
    }

    /// The word the model knows the thing `target` of kind `K` by, leased
    /// now when it has none.
    ///
    /// # Errors
    /// [`ChatDbError::IdsExhausted`] when every word is leased; storage
    /// failures otherwise.
    pub async fn alias<K: AliasKind>(&self, target: &str) -> Result<String, ChatDbError> {
        if let Some(word) = self.live_alias::<K>(target).await? {
            return Ok(word);
        }
        let lease = self.namespace.ids.lease()?;
        // The writer connection orders this against a concurrent alias of the
        // same thing: the row another caller made live first wins, and this
        // lease goes back.
        let stored = query!(
            self.db,
            "INSERT INTO model_aliases (kind, target, word, live) VALUES (?, ?, ?, 1) ON CONFLICT(kind, target) DO UPDATE SET word = excluded.word, live = 1 WHERE live = 0 RETURNING word",
            K::KIND,
            target,
            lease.as_str()
        )
        .fetch_optional::<WordRow>()
        .await?;
        if stored.is_some() {
            let word = lease.to_string();
            self.namespace
                .held()
                .insert((K::KIND, target.to_string()), lease);
            return Ok(word);
        }
        drop(lease);
        self.live_alias::<K>(target)
            .await?
            .ok_or_else(|| ChatDbError::RowNotFound(format!("{} alias of {target}", K::KIND)))
    }

    /// The word the model knows `target` by, whether or not the thing can
    /// still be addressed: its alias, live or retired, or one leased now when
    /// it never had one, retired at once when the thing already ended.
    ///
    /// # Errors
    /// [`ChatDbError::IdsExhausted`] when every word is leased; storage
    /// failures otherwise.
    pub async fn word_of<K: AliasKind>(&self, target: &str) -> Result<String, ChatDbError> {
        if let Some(row) = query!(
            self.db,
            "SELECT word FROM model_aliases WHERE kind = ? AND target = ?",
            K::KIND,
            target
        )
        .fetch_optional::<WordRow>()
        .await?
        {
            return Ok(row.word);
        }
        let word = self.alias::<K>(target).await?;
        self.retire_ended_aliases().await?;
        Ok(word)
    }

    /// The live word of `target`, if it has one.
    async fn live_alias<K: AliasKind>(&self, target: &str) -> Result<Option<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT word FROM model_aliases WHERE kind = ? AND target = ? AND live = 1",
            K::KIND,
            target
        )
        .fetch_optional::<WordRow>()
        .await?
        .map(|row| row.word))
    }

    /// The thing of kind `K` whose live alias is `word`.
    ///
    /// # Errors
    /// Storage failures.
    pub async fn resolve_alias<K: AliasKind>(
        &self,
        word: &str,
    ) -> Result<Option<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT target FROM model_aliases WHERE kind = ? AND word = ? AND live = 1",
            K::KIND,
            word
        )
        .fetch_optional::<TargetRow>()
        .await?
        .map(|row| row.target))
    }

    /// Every thing of kind `K` that `word` has named, live or retired.
    ///
    /// # Errors
    /// Storage failures.
    pub async fn named_by<K: AliasKind>(&self, word: &str) -> Result<Vec<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT target FROM model_aliases WHERE kind = ? AND word = ?",
            K::KIND,
            word
        )
        .fetch_all::<TargetRow>()
        .await?
        .into_iter()
        .map(|row| row.target)
        .collect())
    }

    /// Retires the alias of every thing that ended, so its word goes back
    /// to the namespace. Every write that ends an aliased thing calls it.
    pub(super) async fn retire_ended_aliases(&self) -> Result<(), ChatDbError> {
        let retired = retire_ended(&self.db).await?;
        let released: Vec<IdLease> = {
            let mut held = self.namespace.held();
            retired
                .into_iter()
                .filter_map(|row| held.remove(&(kind_named(&row.kind), row.target)))
                .collect()
        };
        drop(released);
        Ok(())
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
        // The same target under each kind, so only the namespace keeps their
        // words apart.
        let agent = db.alias::<AgentAlias>("one").await.unwrap();
        let question = db.alias::<QuestionAlias>("one").await.unwrap();
        let task = db.alias::<TaskAlias>("one").await.unwrap();
        // Background jobs, stored outputs, subagents and tunnels lease from
        // the same allocator.
        let job = db.model_ids().lease().unwrap();
        let tunnel = db.model_ids().lease().unwrap();
        let words = [
            agent.as_str(),
            question.as_str(),
            task.as_str(),
            job.as_str(),
            tunnel.as_str(),
        ];
        assert_eq!(
            words.iter().collect::<BTreeSet<_>>().len(),
            words.len(),
            "{words:?}"
        );
    }

    #[tokio::test]
    async fn an_alias_is_stable_until_its_thing_ends_and_then_its_word_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).await;
        db.create_session("s1", None, None).await.unwrap();
        let word = db.alias::<AgentAlias>("s1").await.unwrap();
        assert_eq!(db.alias::<AgentAlias>("s1").await.unwrap(), word);
        assert_eq!(
            db.resolve_alias::<AgentAlias>(&word)
                .await
                .unwrap()
                .as_deref(),
            Some("s1")
        );
        assert_eq!(
            db.resolve_alias::<QuestionAlias>(&word).await.unwrap(),
            None
        );
        assert!(db.model_ids().claim(&word).is_err(), "the word is leased");

        db.delete_session("s1").await.unwrap();
        assert_eq!(db.resolve_alias::<AgentAlias>(&word).await.unwrap(), None);
        assert_eq!(db.named_by::<AgentAlias>(&word).await.unwrap(), ["s1"]);
        assert!(db.model_ids().claim(&word).is_ok(), "the word went back");
    }

    #[tokio::test]
    async fn a_live_alias_names_the_same_thing_after_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(dir.path()).await;
        db.create_session("s1", None, None).await.unwrap();
        let word = db.alias::<AgentAlias>("s1").await.unwrap();
        // An alias of a session that does not exist is retired on open.
        let gone = db.alias::<AgentAlias>("absent").await.unwrap();
        drop(db);

        let db = open(dir.path()).await;
        assert_eq!(db.alias::<AgentAlias>("s1").await.unwrap(), word);
        assert!(db.model_ids().claim(&word).is_err(), "leased again on open");
        assert_eq!(db.resolve_alias::<AgentAlias>(&gone).await.unwrap(), None);
        assert!(db.model_ids().claim(&gone).is_ok());
    }
}
