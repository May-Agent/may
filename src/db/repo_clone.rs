//! The host's registry of each session's repository clones.
//!
//! The session directory is agent-writable, so clone locations and sources
//! live here, where only the host writes.

use super::{ChatDb, ChatDbError, query};

/// One `session_repo_clones` row.
#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct RepoCloneRow {
    /// Repository identity the clone serves.
    pub repo_key: String,
    /// Clone directory name under the session's `repos/`.
    pub dir: String,
    /// Local checkout path or remote URL the clone came from.
    pub source: String,
    /// Whether the clone came from a local checkout.
    pub local: bool,
}

impl ChatDb {
    /// Every clone `session_id` holds.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_repo_clones(
        &self,
        session_id: &str,
    ) -> Result<Vec<RepoCloneRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT repo_key, dir, source, local FROM session_repo_clones WHERE session_id = ? ORDER BY rowid",
            session_id
        )
        .fetch_all::<RepoCloneRow>()
        .await?)
    }

    /// Record a clone of `session_id`.
    ///
    /// # Errors
    /// Fails on database errors, including a second clone for the same
    /// repository identity.
    pub async fn insert_repo_clone(
        &self,
        session_id: &str,
        row: &RepoCloneRow,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO session_repo_clones (session_id, repo_key, dir, source, local) VALUES (?, ?, ?, ?, ?)",
            session_id,
            row.repo_key.as_str(),
            row.dir.as_str(),
            row.source.as_str(),
            row.local
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Forget the clone in directory `dir` of `session_id`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn delete_repo_clone(&self, session_id: &str, dir: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM session_repo_clones WHERE session_id = ? AND dir = ?",
            session_id,
            dir
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Forget every clone of `session_id`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn delete_session_repo_clones(&self, session_id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM session_repo_clones WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Forget every session's clones.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn delete_all_repo_clones(&self) -> Result<(), ChatDbError> {
        query!(self.db, "DELETE FROM session_repo_clones")
            .execute()
            .await?;
        Ok(())
    }
}
