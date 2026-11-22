//! Tool-call and user-wait intervals of a session, for the `/usage`
//! wall-clock split.
//!
//! A tool call is appended by the turn runner when it ends. A wait on the
//! user is recorded by triggers on the pending-prompt tables, so every way a
//! prompt leaves them (answered, cancelled, withdrawn, replaced, settled
//! with its run) ends the wait in the same transaction that clears it.

use skyzen_services::Db as SkyDb;

use may_protocol::wall_clock::{Interval, IntervalSet};

use super::{
    ChatDb, ChatDbError,
    db_sql::{self, SESSION_INTERVALS_SCHEMA},
    entity::session_intervals,
    query,
};

const TOOL: &str = "tool";
const USER_WAIT: &str = "user_wait";

/// A session's recorded intervals.
#[derive(Debug, Clone, Default)]
pub struct SessionIntervals {
    /// Tool calls.
    pub tools: IntervalSet,
    /// Waits on the user.
    pub user_waits: IntervalSet,
}

impl ChatDb {
    /// Install the triggers that record a wait when a pending prompt is
    /// cleared.
    pub(super) async fn ensure_session_interval_triggers(db: &SkyDb) -> Result<(), ChatDbError> {
        for statement in db_sql::blocks(SESSION_INTERVALS_SCHEMA) {
            query!(db, statement).execute().await?;
        }
        Ok(())
    }

    /// Record that a tool call of `session_id` ran for `interval`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the insert fails.
    pub async fn append_tool_interval(
        &self,
        session_id: &str,
        interval: Interval,
    ) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO session_intervals (session_id, kind, start_ms, end_ms) VALUES (?, ?, ?, ?)",
            session_id,
            TOOL,
            interval.start_ms(),
            interval.end_ms()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The tool-call and user-wait intervals recorded for `session_id`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    pub async fn session_intervals(
        &self,
        session_id: &str,
    ) -> Result<SessionIntervals, ChatDbError> {
        let rows = query!(
            self.db,
            "SELECT * FROM session_intervals WHERE session_id = ?",
            session_id
        )
        .fetch_all::<session_intervals::Model>()
        .await?;
        let spans = |kind: &str| {
            IntervalSet::new(
                rows.iter()
                    .filter(|row| row.kind == kind)
                    .filter_map(|row| Interval::new(row.start_ms, row.end_ms)),
            )
        };
        Ok(SessionIntervals {
            tools: spans(TOOL),
            user_waits: spans(USER_WAIT),
        })
    }
}
