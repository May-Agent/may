//! How the previous daemon ended, and what an unexpected end owes the
//! sessions it was working for.
//!
//! A daemon records its start, and its stop once its shutdown path drained
//! the runs; a start that finds no recorded stop follows an unexpected exit.
//! The sessions that exit interrupted — a run in flight, or background
//! commands and subagent runs alive — are owed one alert that it happened.

use std::collections::BTreeSet;

use super::{ChatDb, ChatDbError, common::StringValueRow, entity::daemon_runs, query};

/// How the daemon before this one ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviousDaemon {
    /// No daemon ran on this database, or the last one stopped through its
    /// shutdown path.
    Stopped,
    /// The last daemon exited without its shutdown path. It led the process
    /// session `process_session`, which every process it started shares.
    Exited {
        /// Id of the process session the exited daemon led.
        process_session: u32,
    },
}

impl ChatDb {
    /// Records that a daemon leading the process session `process_session`
    /// started, and returns how the one before it ended.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure or a malformed row.
    pub async fn record_daemon_start(
        &self,
        process_session: u32,
    ) -> Result<PreviousDaemon, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let previous = query!(tx, "SELECT * FROM daemon_runs WHERE id = 1")
            .fetch_optional::<daemon_runs::Model>()
            .await?;
        query!(
            tx,
            "INSERT INTO daemon_runs (id, process_session, stopped, started_at) VALUES (1, ?, 0, ?) ON CONFLICT(id) DO UPDATE SET process_session = excluded.process_session, stopped = 0, started_at = excluded.started_at",
            i64::from(process_session),
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(match previous {
            None => PreviousDaemon::Stopped,
            Some(row) if row.stopped => PreviousDaemon::Stopped,
            Some(row) => PreviousDaemon::Exited {
                process_session: u32::try_from(row.process_session).map_err(|_| {
                    super::storage_error(format!(
                        "stored daemon process session {} is not a process id",
                        row.process_session
                    ))
                })?,
            },
        })
    }

    /// Records that the running daemon stopped through its shutdown path.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn record_daemon_stop(&self) -> Result<(), ChatDbError> {
        query!(self.db, "UPDATE daemon_runs SET stopped = 1 WHERE id = 1")
            .execute()
            .await?;
        Ok(())
    }

    /// Replaces the set of sessions whose agent has background work alive:
    /// a terminal job or subagent run in flight, or a wake being served.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn set_sessions_with_background(
        &self,
        sessions: &BTreeSet<String>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut tx = self.db.begin().await?;
        query!(tx, "DELETE FROM session_background_live")
            .execute()
            .await?;
        for session_id in sessions {
            query!(
                tx,
                "INSERT INTO session_background_live (session_id, updated_at) VALUES (?, ?)",
                session_id.as_str(),
                now.as_str()
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The sessions owed the alert that the daemon exited unexpectedly.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn sessions_owed_daemon_exit_alert(&self) -> Result<Vec<String>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT session_id AS value FROM session_daemon_exit_alerts ORDER BY created_at"
        )
        .fetch_all::<StringValueRow>()
        .await?
        .into_iter()
        .map(|row| row.value)
        .collect())
    }

    /// Atomically consumes the daemon-exit alert `session_id` is owed;
    /// whether it was owed one.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn take_daemon_exit_alert(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let result = query!(
            self.db,
            "DELETE FROM session_daemon_exit_alerts WHERE session_id = ?",
            session_id
        )
        .execute()
        .await?;
        Ok(result.rows_written > 0)
    }

    /// Owes `session_id` the daemon-exit alert again: the turn that consumed
    /// it was withdrawn before the model saw it.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] on storage failure.
    pub async fn restore_daemon_exit_alert(&self, session_id: &str) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT OR IGNORE INTO session_daemon_exit_alerts (session_id, created_at) VALUES (?, ?)",
            session_id,
            jiff::Timestamp::now().to_string()
        )
        .execute()
        .await?;
        Ok(())
    }
}
