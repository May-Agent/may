//! Run-scoped commits on [`ChatDb`].
//!
//! One run owns a session at a time; [`SessionRuns`](crate::session::SessionRuns)
//! enforces that in the daemon's memory. Each run stamps its random
//! [`RunFence::run_id`] on the session's runtime row when it starts, and every
//! write the run makes goes through [`ChatDb::begin_fenced`] (or a method
//! built on it), which commits only while that id still names the run — so a
//! write issued after the run ended, or by a run the next run superseded,
//! cannot land.

use skyzen_services::DbTransaction;

use crate::session::SessionRuntimeState;

use super::{ChatDb, ChatDbError, query};

/// Identifies one run's hold on its session.
///
/// `run_id` is a random token per run, stamped on `session_runtimes.run_id`
/// while the run lives; fenced commits condition on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFence {
    /// Session the run belongs to.
    pub session_id: String,
    /// Random token of this run.
    pub run_id: String,
}

impl ChatDb {
    /// Stamp `fence`'s run id on the session's runtime row and mark it
    /// `Running`, creating the row when missing. The stamp is conditional
    /// on no run id being recorded, so it backs the process registry's
    /// exclusion: only a superseded or cleared claim can be overwritten.
    /// Returns whether the claim landed.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn claim_session_run(&self, fence: &RunFence) -> Result<bool, ChatDbError> {
        let claimed = query!(
            self.db,
            "INSERT INTO session_runtimes (session_id, state, run_id, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, ?, ?, 0, 0, ?) ON CONFLICT(session_id) DO UPDATE SET run_id = excluded.run_id, state = excluded.state, updated_at = excluded.updated_at WHERE session_runtimes.run_id IS NULL",
            fence.session_id.as_str(),
            SessionRuntimeState::Running.as_str(),
            fence.run_id.as_str(),
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        Ok(claimed.rows_written > 0)
    }

    /// Begin a transaction whose commit is conditioned on the run: its
    /// first statement touches the runtime row only while `fence` still
    /// names the run, which also takes SQLite's write lock up front.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RunLost`] when `fence` no longer names the
    /// session's run, or [`ChatDbError`] on storage failure.
    pub(super) async fn begin_fenced(
        &self,
        fence: &RunFence,
    ) -> Result<DbTransaction, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let held = query!(
            tx,
            "UPDATE session_runtimes SET run_id = run_id WHERE session_id = ? AND run_id = ?",
            fence.session_id.as_str(),
            fence.run_id.as_str()
        )
        .execute()
        .await?;
        if held.rows_written == 0 {
            tx.rollback().await?;
            return Err(ChatDbError::RunLost {
                session_id: fence.session_id.clone(),
            });
        }
        Ok(tx)
    }

    /// Settle the run's session and clear its run id, in one fenced
    /// transaction: every waiting request of the session is resolved, its
    /// terminal interaction cleared and its state set `Idle`.
    ///
    /// Returns `false`, touching nothing, when `fence` no longer names the
    /// run: the session's state then belongs to the run that superseded it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn finish_session_run(&self, fence: &RunFence) -> Result<bool, ChatDbError> {
        let mut tx = match self.begin_fenced(fence).await {
            Ok(tx) => tx,
            Err(ChatDbError::RunLost { .. }) => return Ok(false),
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
            "UPDATE session_runtimes SET state = 'idle', run_id = NULL, terminal_task_id = NULL, terminal_output_file = NULL, terminal_output_preview = NULL, terminal_input_notice = NULL, terminal_background_reason = NULL, updated_at = ? WHERE session_id = ?",
            jiff::Timestamp::now().to_string(),
            session_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Set the session's base runtime state as part of a run: committed only
    /// while `fence` still names the session's run.
    ///
    /// # Errors
    /// Returns [`ChatDbError::RunLost`] when the run ended or was
    /// superseded, or [`ChatDbError`] on storage failure.
    pub async fn set_session_runtime_state_as_run(
        &self,
        fence: &RunFence,
        state: SessionRuntimeState,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.begin_fenced(fence).await?;
        query!(
            tx,
            "INSERT INTO session_runtimes (session_id, state, run_id, browser_takeover_active, browser_takeover_alert_pending, updated_at) VALUES (?, ?, ?, 0, 0, ?) ON CONFLICT(session_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
            fence.session_id.as_str(),
            state.as_str(),
            fence.run_id.as_str(),
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pop the next pending message for the run that holds the session.
    /// Only an active interaction (a browser takeover) holds the queue back.
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
}

#[cfg(test)]
mod tests {
    use super::{ChatDb, ChatDbError, RunFence};
    use crate::session::SessionRuntimeState;
    use tempfile::TempDir;

    fn fence(session_id: &str, run_id: &str) -> RunFence {
        RunFence {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
        }
    }

    #[tokio::test]
    async fn a_fenced_commit_dies_with_its_run() {
        let dir = TempDir::new().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");

        let run = fence("session", "run-1");
        assert!(db.claim_session_run(&run).await.expect("claim"));
        db.set_session_runtime_state_as_run(&run, SessionRuntimeState::Running)
            .await
            .expect("set running");

        let next = fence("session", "run-2");
        assert!(
            !db.claim_session_run(&next).await.expect("claim"),
            "a held session refuses a second claim"
        );
        assert!(db.finish_session_run(&run).await.expect("settle"));
        assert!(db.claim_session_run(&next).await.expect("supersede"));
        let error = db
            .set_session_runtime_state_as_run(&run, SessionRuntimeState::Idle)
            .await
            .expect_err("stale run must not commit");
        assert!(matches!(error, ChatDbError::RunLost { .. }));
    }

    #[tokio::test]
    async fn finish_settles_the_session_and_frees_the_run_id() {
        let dir = TempDir::new().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");

        db.create_session("session", Some("Session"), Some("test-model"))
            .await
            .expect("create session");
        let run = fence("session", "run-1");
        db.claim_session_run(&run).await.expect("claim");
        db.set_session_runtime_state_as_run(&run, SessionRuntimeState::Running)
            .await
            .expect("set running");

        assert!(db.finish_session_run(&run).await.expect("finish"));
        // A second finish finds the run id cleared.
        assert!(!db.finish_session_run(&run).await.expect("finish"));

        let runtime = db
            .get_session_runtime("session")
            .await
            .expect("runtime read")
            .expect("runtime row");
        assert_eq!(runtime.state, SessionRuntimeState::Idle);
    }
}
