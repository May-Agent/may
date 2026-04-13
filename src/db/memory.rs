//! Push-subscription and profile-memory queries on [`ChatDb`].

use super::entity::{profile_memories, profile_memory_jobs, push_subscriptions};
use super::{ChatDb, ChatDbError, query};

impl ChatDb {
    /// Store a push subscription from a browser.
    pub async fn add_push_subscription(
        &self,
        id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
    ) -> Result<push_subscriptions::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO push_subscriptions (id, endpoint, p256dh, auth, created_at) VALUES (?, ?, ?, ?, ?)",
            id,
            endpoint,
            p256dh,
            auth,
            now.clone()
        )
        .execute()
        .await?;
        Ok(push_subscriptions::Model {
            id: id.to_string(),
            endpoint: endpoint.to_string(),
            p256dh: p256dh.to_string(),
            auth: auth.to_string(),
            created_at: now,
        })
    }

    /// Remove a push subscription by endpoint.
    pub async fn remove_push_subscription(&self, endpoint: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "DELETE FROM push_subscriptions WHERE endpoint = ?",
            endpoint
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List all push subscriptions.
    pub async fn list_push_subscriptions(
        &self,
    ) -> Result<Vec<push_subscriptions::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM push_subscriptions ORDER BY created_at DESC"
        )
        .fetch_all::<push_subscriptions::Model>()
        .await?)
    }

    // ── Profile memories ──────────────────────────────────

    /// Get a profile memory by profile ID and type.
    pub async fn get_profile_memory(
        &self,
        profile_id: &str,
        memory_type: &str,
    ) -> Result<Option<profile_memories::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM profile_memories WHERE profile_id = ? AND memory_type = ? LIMIT 1",
            profile_id,
            memory_type
        )
        .fetch_optional::<profile_memories::Model>()
        .await?)
    }

    /// Get all memories for a profile.
    pub async fn get_profile_memories(
        &self,
        profile_id: &str,
    ) -> Result<Vec<profile_memories::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM profile_memories WHERE profile_id = ? ORDER BY updated_at DESC",
            profile_id
        )
        .fetch_all::<profile_memories::Model>()
        .await?)
    }

    /// Upsert a profile memory (insert or replace).
    pub async fn upsert_profile_memory(
        &self,
        profile_id: &str,
        memory_type: &str,
        content: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let existing = self.get_profile_memory(profile_id, memory_type).await?;

        if let Some(existing) = existing {
            query!(
                self.db,
                "UPDATE profile_memories SET content = ?, updated_at = ? WHERE id = ?",
                content,
                now,
                existing.id
            )
            .execute()
            .await?;
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            query!(
                self.db,
                "INSERT INTO profile_memories (id, profile_id, memory_type, content, updated_at) VALUES (?, ?, ?, ?, ?)",
                id,
                profile_id,
                memory_type,
                content,
                now
            )
            .execute()
            .await?;
        }

        Ok(())
    }

    /// Mark a profile memory refresh job dirty.
    pub async fn mark_profile_memory_job_dirty(&self, profile_id: &str) -> Result<(), ChatDbError> {
        let updated_at = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO profile_memory_jobs (profile_id, dirty, last_processed_at, updated_at) VALUES (?, ?, ?, ?) ON CONFLICT(profile_id) DO UPDATE SET dirty = excluded.dirty, updated_at = excluded.updated_at",
            profile_id,
            true,
            Option::<String>::None,
            updated_at
        )
        .execute()
        .await?;
        Ok(())
    }

    /// List profile memory jobs still marked dirty.
    pub async fn list_dirty_profile_memory_jobs(
        &self,
    ) -> Result<Vec<profile_memory_jobs::Model>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM profile_memory_jobs WHERE dirty = ? ORDER BY updated_at ASC",
            true
        )
        .fetch_all::<profile_memory_jobs::Model>()
        .await?)
    }

    /// Mark a profile memory refresh job processed.
    pub async fn mark_profile_memory_job_processed(
        &self,
        profile_id: &str,
        expected_updated_at: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "UPDATE profile_memory_jobs SET dirty = ?, last_processed_at = ?, updated_at = ? WHERE profile_id = ? AND updated_at = ?",
            false,
            Some(now.clone()),
            now,
            profile_id,
            expected_updated_at
        )
        .execute()
        .await?;
        Ok(())
    }

    // ── Session Tasks (DB-backed plan/todo) ──────────────────

    /// Get the tasks content for a session.
    pub async fn get_session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, ChatDbError> {
        #[derive(serde::Deserialize)]
        struct Row {
            content: String,
        }
        Ok(query!(
            self.db,
            "SELECT content FROM session_tasks WHERE session_id = ?",
            session_id
        )
        .fetch_optional::<Row>()
        .await?
        .map(|r| r.content))
    }

    /// Upsert the tasks content for a session.
    pub async fn set_session_tasks(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO session_tasks (session_id, content, updated_at) VALUES (?, ?, ?) ON CONFLICT(session_id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
            session_id,
            content,
            now
        )
        .execute()
        .await?;
        Ok(())
    }

    // ── Global Memory (DB-backed MEMORY.md replacement) ──────

    /// Get the global memory content.
    pub async fn get_global_memory(&self) -> Result<Option<String>, ChatDbError> {
        #[derive(serde::Deserialize)]
        struct Row {
            content: String,
        }
        Ok(query!(
            self.db,
            "SELECT content FROM global_memory WHERE id = 1"
        )
        .fetch_optional::<Row>()
        .await?
        .map(|r| r.content))
    }

    /// Upsert the global memory content.
    pub async fn set_global_memory(&self, content: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        query!(
            self.db,
            "INSERT INTO global_memory (id, content, updated_at) VALUES (1, ?, ?) ON CONFLICT(id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
            content,
            now
        )
        .execute()
        .await?;
        Ok(())
    }
}
