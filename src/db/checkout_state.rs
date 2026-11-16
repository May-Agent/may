//! Per-session tracking of daemon default-branch checkouts and read files.
//!
//! Tracks the last default-branch commit each session observed per GitHub
//! repository alongside the repo-relative file paths read via terminal
//! commands inside that daemon checkout, so upstream updates while a session
//! is idle or restarted collapse into a single request-time alert naming only
//! read files that changed.

use super::{ChatDb, ChatDbError, query};

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct SqlRow {
    sql: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct CommitRow {
    commit_sha: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct SeenCheckoutRow {
    repo_key: String,
    commit_sha: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct PathRow {
    path: String,
}

impl ChatDb {
    /// Migrates pre-existing `session_repo_clones` tables whose primary key
    /// was `(session_id, repo_key)` to `(session_id, dir)` so a session may
    /// hold multiple concurrent workspaces on the same repository.
    pub(super) async fn migrate_session_repo_clones(
        db: &skyzen_services::Db,
    ) -> Result<(), ChatDbError> {
        let existing = query!(
            db,
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'session_repo_clones'"
        )
        .fetch_optional::<SqlRow>()
        .await?;
        let Some(row) = existing else {
            return Ok(());
        };
        if !row.sql.contains("PRIMARY KEY (session_id, repo_key)") {
            return Ok(());
        }
        let mut tx = db.begin().await?;
        query!(
            tx,
            "CREATE TABLE session_repo_clones_new (session_id TEXT NOT NULL, repo_key TEXT NOT NULL, dir TEXT NOT NULL, source TEXT NOT NULL, local BOOLEAN NOT NULL, base_sha TEXT, PRIMARY KEY (session_id, dir))"
        )
        .execute()
        .await?;
        query!(
            tx,
            "INSERT INTO session_repo_clones_new (session_id, repo_key, dir, source, local, base_sha) SELECT session_id, repo_key, dir, source, local, base_sha FROM session_repo_clones"
        )
        .execute()
        .await?;
        query!(tx, "DROP TABLE session_repo_clones")
            .execute()
            .await?;
        query!(
            tx,
            "ALTER TABLE session_repo_clones_new RENAME TO session_repo_clones"
        )
        .execute()
        .await?;
        query!(
            tx,
            "CREATE INDEX IF NOT EXISTS idx_session_repo_clones_repo ON session_repo_clones(session_id, repo_key)"
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Last default-branch commit observed by `session_id` on `repo_key`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn seen_checkout_commit(
        &self,
        session_id: &str,
        repo_key: &str,
    ) -> Result<Option<String>, ChatDbError> {
        let row = query!(
            self.db,
            "SELECT commit_sha FROM session_checkout_seen WHERE session_id = ? AND repo_key = ?",
            session_id,
            repo_key
        )
        .fetch_optional::<CommitRow>()
        .await?;
        Ok(row.map(|r| r.commit_sha))
    }

    /// Seed the initial seen commit for `(session_id, repo_key)` only if not
    /// already recorded, preserving prior state across daemon restarts.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn seed_seen_checkout_commit(
        &self,
        session_id: &str,
        repo_key: &str,
        commit_sha: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT OR IGNORE INTO session_checkout_seen (session_id, repo_key, commit_sha) VALUES (?, ?, ?)",
            session_id,
            repo_key,
            commit_sha
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Record the latest default-branch commit seen by `session_id` on `repo_key`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn set_seen_checkout_commit(
        &self,
        session_id: &str,
        repo_key: &str,
        commit_sha: &str,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO session_checkout_seen (session_id, repo_key, commit_sha) VALUES (?, ?, ?) ON CONFLICT (session_id, repo_key) DO UPDATE SET commit_sha = excluded.commit_sha",
            session_id,
            repo_key,
            commit_sha
        )
        .execute()
        .await?;
        Ok(())
    }

    /// Record repo-relative file paths read by `session_id` in `repo_key`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn record_checkout_read_files(
        &self,
        session_id: &str,
        repo_key: &str,
        paths: &[String],
    ) -> Result<(), ChatDbError> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        for path in paths {
            query!(
                tx,
                "INSERT OR IGNORE INTO session_checkout_files (session_id, repo_key, path) VALUES (?, ?, ?)",
                session_id,
                repo_key,
                path.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Every `(repo_key, seen_commit_sha)` tracked for `session_id`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_checkout_seen_repos(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT repo_key, commit_sha FROM session_checkout_seen WHERE session_id = ? ORDER BY repo_key",
            session_id
        )
        .fetch_all::<SeenCheckoutRow>()
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.repo_key, r.commit_sha))
            .collect())
    }

    /// Repo-relative file paths previously read by `session_id` on `repo_key`.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn list_checkout_read_files(
        &self,
        session_id: &str,
        repo_key: &str,
    ) -> Result<Vec<String>, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT path FROM session_checkout_files WHERE session_id = ? AND repo_key = ? ORDER BY path",
            session_id,
            repo_key
        )
        .fetch_all::<PathRow>()
        .await?;
        Ok(rows.into_iter().map(|r| r.path).collect())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::db::{ChatDb, RepoCloneRow, query};

    #[tokio::test]
    async fn migrates_legacy_session_repo_clones_pk_to_dir() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("history.db");
        {
            let raw = ChatDb::connect(&db_path).await.unwrap();
            query!(
                raw,
                "CREATE TABLE session_repo_clones (session_id TEXT NOT NULL, repo_key TEXT NOT NULL, dir TEXT NOT NULL, source TEXT NOT NULL, local BOOLEAN NOT NULL, base_sha TEXT, PRIMARY KEY (session_id, repo_key))"
            )
            .execute()
            .await
            .unwrap();
            query!(
                raw,
                "INSERT INTO session_repo_clones (session_id, repo_key, dir, source, local, base_sha) VALUES ('s1', 'May-Agent/may', 'may-11111111', 'https://github.com/May-Agent/may.git', 0, NULL)"
            )
            .execute()
            .await
            .unwrap();
        }

        let db = ChatDb::open(&db_path).await.unwrap();
        db.insert_repo_clone(
            "s1",
            &RepoCloneRow {
                repo_key: "May-Agent/may".to_string(),
                dir: "may-11111111-2".to_string(),
                source: "https://github.com/May-Agent/may.git".to_string(),
                local: false,
                base_sha: None,
            },
        )
        .await
        .unwrap();

        let rows = db.list_repo_clones("s1").await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].dir, "may-11111111");
        assert_eq!(rows[1].dir, "may-11111111-2");
    }
}
