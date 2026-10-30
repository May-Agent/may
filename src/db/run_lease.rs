//! Fenced session run leases on [`ChatDb`].
//!
//! A lease row names the run (`owner`) executing a session and counts
//! acquisitions (`generation`). Acquisition is one conditional upsert, so
//! concurrent claimants across handles and processes contend on SQLite's
//! write lock and exactly one wins. Every write a run makes goes through
//! [`ChatDb::begin_fenced`] (or a method built on it), which commits only
//! while the run still holds the lease, so a holder that lost it to a
//! takeover cannot write.

use skyzen_services::DbTransaction;

use crate::session::SessionRuntimeState;

use super::{ChatDb, ChatDbError, query};

/// Identifies one acquisition of a session's run lease.
///
/// Two runs never share a fence: `owner` is a random token per run and
/// `generation` grows with every acquisition of the session's lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFence {
    /// Session the lease belongs to.
    pub session_id: String,
    /// Random token of the run holding the lease.
    pub owner: String,
    /// Acquisition count of the lease when the run took it.
    pub generation: i64,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct GenerationRow {
    generation: i64,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
struct CancellationRow {
    cancellation_requested: bool,
}

impl ChatDb {
    /// Take the session's run lease for `owner` and mark the session
    /// `Running`, in one transaction.
    ///
    /// The lease is taken only while it is free or its holder's heartbeat is
    /// older than `expired_before_ms`. Returns the fence of the new
    /// acquisition, or `None` when a live run holds the lease.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn acquire_session_run_lease(
        &self,
        session_id: &str,
        owner: &str,
        now_ms: i64,
        expired_before_ms: i64,
    ) -> Result<Option<RunFence>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let acquired = query!(
            tx,
            "INSERT INTO session_run_leases (session_id, owner, generation, heartbeat_at, cancellation_requested) VALUES (?, ?, 1, ?, 0) ON CONFLICT(session_id) DO UPDATE SET owner = excluded.owner, generation = session_run_leases.generation + 1, heartbeat_at = excluded.heartbeat_at, cancellation_requested = 0 WHERE session_run_leases.owner IS NULL OR session_run_leases.heartbeat_at < ? RETURNING generation",
            session_id,
            owner,
            now_ms,
            expired_before_ms
        )
        .fetch_optional::<GenerationRow>()
        .await?;
        let Some(acquired) = acquired else {
            tx.rollback().await?;
            return Ok(None);
        };
        query!(
            tx,
            "INSERT INTO session_runtimes (session_id, state, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, ?, 0, 0, ?) ON CONFLICT(session_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            session_id,
            SessionRuntimeState::Running.as_str(),
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(Some(RunFence {
            session_id: session_id.to_string(),
            owner: owner.to_string(),
            generation: acquired.generation,
        }))
    }

    /// Renew the lease's heartbeat. Returns whether cancellation was
    /// requested, or `None` when the fence no longer holds the lease.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn renew_session_run_lease(
        &self,
        fence: &RunFence,
        now_ms: i64,
    ) -> Result<Option<bool>, ChatDbError> {
        Ok(query!(
            self.db,
            "UPDATE session_run_leases SET heartbeat_at = ? WHERE session_id = ? AND owner = ? AND generation = ? RETURNING cancellation_requested",
            now_ms,
            fence.session_id.as_str(),
            fence.owner.as_str(),
            fence.generation
        )
        .fetch_optional::<CancellationRow>()
        .await?
        .map(|row| row.cancellation_requested))
    }

    /// Release the lease. Does nothing, and returns `false`, unless the
    /// fence still holds it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn release_session_run_lease(&self, fence: &RunFence) -> Result<bool, ChatDbError> {
        let released = query!(
            self.db,
            "UPDATE session_run_leases SET owner = NULL, cancellation_requested = 0 WHERE session_id = ? AND owner = ? AND generation = ?",
            fence.session_id.as_str(),
            fence.owner.as_str(),
            fence.generation
        )
        .execute()
        .await?;
        Ok(released.rows_written > 0)
    }

    /// Settle the run's session and release its lease, in one fenced
    /// transaction: every waiting request of the session is resolved, its
    /// terminal interaction cleared, its state set `Idle`, then the lease
    /// freed.
    ///
    /// Returns `false`, touching nothing, when the fence no longer holds the
    /// lease: the session's state then belongs to the new holder.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn finish_session_run(&self, fence: &RunFence) -> Result<bool, ChatDbError> {
        let mut tx = match self.begin_fenced(fence).await {
            Ok(tx) => tx,
            Err(ChatDbError::RunLeaseLost { .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        let session_id = fence.session_id.as_str();
        for sql in [
            "DELETE FROM session_pending_permissions WHERE session_id = ?",
            "DELETE FROM session_pending_workspace WHERE session_id = ?",
            "DELETE FROM session_pending_ask_user_question_options_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1)",
            "DELETE FROM session_pending_ask_user_questions_v2 WHERE request_id IN (SELECT request_id FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1)",
            "DELETE FROM session_pending_ask_user_requests_v2 WHERE session_id = ? AND blocking = 1",
        ] {
            query!(tx, sql, session_id).execute().await?;
        }
        query!(
            tx,
            "UPDATE session_runtimes SET state = 'idle', terminal_task_id = NULL, terminal_output_file = NULL, terminal_output_preview = NULL, terminal_input_notice = NULL, terminal_background_reason = NULL, updated_at = ? WHERE session_id = ?",
            jiff::Timestamp::now().to_string(),
            session_id
        )
        .execute()
        .await?;
        query!(
            tx,
            "UPDATE session_run_leases SET owner = NULL, cancellation_requested = 0 WHERE session_id = ? AND owner = ? AND generation = ?",
            session_id,
            fence.owner.as_str(),
            fence.generation
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Begin a transaction whose commit is conditioned on the fence: its
    /// first statement touches the lease row only while `fence` still holds
    /// it, which also takes SQLite's write lock up front.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RunLeaseLost`] when the fence no longer holds
    /// the lease, or [`ChatDbError`] on storage failure.
    pub(super) async fn begin_fenced(
        &self,
        fence: &RunFence,
    ) -> Result<DbTransaction, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let held = query!(
            tx,
            "UPDATE session_run_leases SET heartbeat_at = max(heartbeat_at, ?) WHERE session_id = ? AND owner = ? AND generation = ?",
            jiff::Timestamp::now().as_millisecond(),
            fence.session_id.as_str(),
            fence.owner.as_str(),
            fence.generation
        )
        .execute()
        .await?;
        if held.rows_written == 0 {
            tx.rollback().await?;
            return Err(ChatDbError::RunLeaseLost {
                session_id: fence.session_id.clone(),
            });
        }
        Ok(tx)
    }

    /// Set the session's base runtime state as part of a run: committed only
    /// while the run's lease `fence` still holds.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RunLeaseLost`] when the lease was taken over or
    /// released, or [`ChatDbError`] on storage failure.
    pub async fn set_session_runtime_state_as_run(
        &self,
        fence: &RunFence,
        state: SessionRuntimeState,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.begin_fenced(fence).await?;
        query!(
            tx,
            "INSERT INTO session_runtimes (session_id, state, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, ?, 0, 0, ?) ON CONFLICT(session_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            fence.session_id.as_str(),
            state.as_str(),
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pop the next pending message for the run that holds the session's
    /// lease. The lease, not the runtime state, gates the dispatch; only an
    /// active interaction (a browser takeover) holds the queue back.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn take_next_pending_message_for_run(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::session::PendingUserMessage>, ChatDbError> {
        let summary = self.load_session_runtime_summary(session_id).await?;
        if summary.interactions.is_active() || !summary.has_pending_messages() {
            return Ok(None);
        }
        self.take_next_pending_message(session_id).await
    }

    /// Ask the session's current holder to stop. Returns whether a lease was
    /// held to receive the request.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn request_session_run_cancel(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let result = query!(
            self.db,
            "UPDATE session_run_leases SET cancellation_requested = 1 WHERE session_id = ? AND owner IS NOT NULL",
            session_id
        )
        .execute()
        .await?;
        Ok(result.rows_written > 0)
    }

    /// Whether a live run holds the session's lease: it has an owner whose
    /// heartbeat is not older than `expired_before_ms`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn session_run_lease_held(
        &self,
        session_id: &str,
        expired_before_ms: i64,
    ) -> Result<bool, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT generation FROM session_run_leases WHERE session_id = ? AND owner IS NOT NULL AND heartbeat_at >= ?",
            session_id,
            expired_before_ms
        )
        .fetch_optional::<GenerationRow>()
        .await?
        .is_some())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{ChatDb, ChatDbError, RunFence};
    use crate::session::{ChatMessage, SessionRuntimeState};

    async fn open_with_session() -> (tempfile::TempDir, ChatDb) {
        let dir = tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("open database");
        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        (dir, db)
    }

    /// A holder whose heartbeat expired loses the lease to a takeover; its
    /// renewal fails and its fenced commit is rejected, while the new
    /// holder's commit lands.
    #[tokio::test]
    async fn expired_holder_is_fenced_out_after_takeover() {
        let (_dir, db) = open_with_session().await;
        let old = db
            .acquire_session_run_lease("session", "old", 1_000, 0)
            .await
            .expect("acquire")
            .expect("free");
        assert_eq!(old.generation, 1);
        // Not yet expired at 1_500 with a cutoff of 900: refused.
        assert!(
            db.acquire_session_run_lease("session", "new", 1_500, 900)
                .await
                .expect("acquire")
                .is_none()
        );
        // Expired at 5_000 with a cutoff of 4_000: taken over.
        let new = db
            .acquire_session_run_lease("session", "new", 5_000, 4_000)
            .await
            .expect("acquire")
            .expect("expired lease is taken over");
        assert_eq!(new.generation, 2);

        assert_eq!(
            db.renew_session_run_lease(&old, 5_100)
                .await
                .expect("renew"),
            None,
            "the expired holder's renewal must fail"
        );
        let rejected = db
            .append_chat_message_as_run(&old, &ChatMessage::user("stale write"))
            .await;
        assert!(
            matches!(rejected, Err(ChatDbError::RunLeaseLost { .. })),
            "the stale holder's commit must be rejected, got {rejected:?}"
        );
        assert!(
            db.get_messages("session")
                .await
                .expect("messages")
                .is_empty(),
            "the rejected commit must leave no row"
        );

        assert_eq!(
            db.renew_session_run_lease(&new, 5_100)
                .await
                .expect("renew"),
            Some(false)
        );
        db.append_chat_message_as_run(&new, &ChatMessage::user("fresh write"))
            .await
            .expect("the current holder's commit lands");
        assert_eq!(db.get_messages("session").await.expect("messages").len(), 1);
    }

    /// Releasing with another run's owner token or an old generation changes
    /// nothing.
    #[tokio::test]
    async fn release_by_a_non_owner_is_a_no_op() {
        let (_dir, db) = open_with_session().await;
        let holder = db
            .acquire_session_run_lease("session", "holder", 1_000, 0)
            .await
            .expect("acquire")
            .expect("free");

        let impostor = RunFence {
            owner: "impostor".to_string(),
            ..holder.clone()
        };
        let stale_generation = RunFence {
            generation: holder.generation - 1,
            ..holder.clone()
        };
        for fence in [&impostor, &stale_generation] {
            assert!(
                !db.release_session_run_lease(fence).await.expect("release"),
                "{fence:?} must not release the lease"
            );
            assert!(
                !db.finish_session_run(fence).await.expect("finish"),
                "{fence:?} must not settle the session"
            );
        }
        assert!(
            db.session_run_lease_held("session", 0).await.expect("held"),
            "the lease must still be held"
        );
        assert_eq!(
            db.get_session_runtime("session")
                .await
                .expect("runtime")
                .expect("row")
                .state,
            SessionRuntimeState::Running,
            "a refused finish must not touch the session"
        );

        assert!(
            db.release_session_run_lease(&holder)
                .await
                .expect("release")
        );
        assert!(
            !db.session_run_lease_held("session", 0).await.expect("held"),
            "the owner's release frees the lease"
        );
    }

    /// The boolean lease table of the previous build is replaced by the
    /// fenced layout when the database is opened.
    #[tokio::test]
    async fn legacy_lease_table_is_recreated_on_open() {
        let dir = tempdir().expect("temp dir");
        let path = dir.path().join("history.db");
        {
            let db = ChatDb::open(&path).await.expect("open");
            super::query!(db.db, "DROP TABLE session_run_leases")
                .execute()
                .await
                .expect("drop");
            super::query!(
                db.db,
                "CREATE TABLE session_run_leases (session_id TEXT PRIMARY KEY, active BOOLEAN NOT NULL, cancellation_requested BOOLEAN NOT NULL, updated_at TEXT NOT NULL)"
            )
            .execute()
            .await
            .expect("legacy table");
            super::query!(
                db.db,
                "INSERT INTO session_run_leases (session_id, active, cancellation_requested, updated_at) VALUES ('session', 1, 0, 'then')"
            )
            .execute()
            .await
            .expect("legacy lease");
        }
        let db = ChatDb::open(&path).await.expect("reopen migrates");
        let fence = db
            .acquire_session_run_lease("session", "owner", 1_000, 0)
            .await
            .expect("acquire")
            .expect("the legacy lease is gone with its table");
        assert_eq!(fence.generation, 1);
    }
}
