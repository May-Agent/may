//! Which git worktrees May is used in, so the daemon keeps them indexed.

use std::path::{Path, PathBuf};

use super::{ChatDb, ChatDbError};

/// One worktree's usage record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, skyzen::FromRow)]
pub struct RepoUsage {
    /// Canonical worktree root.
    pub worktree: String,
    /// Unix seconds of the latest use.
    pub last_used_at: i64,
    /// Number of recorded uses.
    pub use_count: i64,
}

impl RepoUsage {
    /// Worktree root as a path.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.worktree)
    }
}

fn unix_now() -> i64 {
    jiff::Timestamp::now().as_second()
}

impl ChatDb {
    /// Records a use of `worktree` now.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the upsert fails.
    pub async fn touch_repo_usage(&self, worktree: &Path) -> Result<(), ChatDbError> {
        self.touch_repo_usage_at(worktree, unix_now()).await
    }

    /// Records a use of `worktree` at `at` (unix seconds).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the upsert fails.
    pub async fn touch_repo_usage_at(&self, worktree: &Path, at: i64) -> Result<(), ChatDbError> {
        self.db
            .query(
                "INSERT INTO repo_usage (worktree, last_used_at, use_count) VALUES (?, ?, 1) \
                 ON CONFLICT(worktree) DO UPDATE SET \
                 last_used_at = MAX(last_used_at, excluded.last_used_at), \
                 use_count = use_count + 1",
            )
            .bind(worktree.display().to_string())
            .bind(at)
            .execute()
            .await?;
        Ok(())
    }

    /// Worktrees used at or after `since` (unix seconds).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn repo_usage_since(&self, since: i64) -> Result<Vec<RepoUsage>, ChatDbError> {
        Ok(self
            .db
            .query("SELECT worktree, last_used_at, use_count FROM repo_usage WHERE last_used_at >= ?")
            .bind(since)
            .fetch_all::<RepoUsage>()
            .await?)
    }

    /// Forgets `worktree`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the delete fails.
    pub async fn forget_repo_usage(&self, worktree: &Path) -> Result<(), ChatDbError> {
        self.db
            .query("DELETE FROM repo_usage WHERE worktree = ?")
            .bind(worktree.display().to_string())
            .execute()
            .await?;
        Ok(())
    }

    /// Drops every record last used before `before` (unix seconds).
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the delete fails.
    pub async fn expire_repo_usage(&self, before: i64) -> Result<(), ChatDbError> {
        self.db
            .query("DELETE FROM repo_usage WHERE last_used_at < ?")
            .bind(before)
            .execute()
            .await?;
        Ok(())
    }
}
