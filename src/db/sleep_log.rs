//! Every `/sleep` with its time, behind the health reminder.

use super::{ChatDb, ChatDbError, entity::sleep_log, query};

impl ChatDb {
    /// Record a `/sleep` run at `at`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the insert fails.
    pub async fn record_sleep(&self, at: jiff::Timestamp) -> Result<(), ChatDbError> {
        query!(
            self.db,
            "INSERT INTO sleep_log (slept_at_ms) VALUES (?)",
            at.as_millisecond()
        )
        .execute()
        .await?;
        Ok(())
    }

    /// The time of every recorded `/sleep`.
    ///
    /// # Errors
    /// Returns [`ChatDbError`] when the query fails.
    ///
    /// # Panics
    /// When a stored time is out of range, which only this module writes.
    pub async fn sleep_times(&self) -> Result<Vec<jiff::Timestamp>, ChatDbError> {
        let rows = query!(self.db, "SELECT * FROM sleep_log ORDER BY slept_at_ms")
            .fetch_all::<sleep_log::Model>()
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                jiff::Timestamp::from_millisecond(row.slept_at_ms)
                    .expect("stored sleep time is in range")
            })
            .collect())
    }
}
