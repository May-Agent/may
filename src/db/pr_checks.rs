//! What the daemon last saw of each live GitHub PR's checks, and the alerts
//! that follow from a change, on [`ChatDb`].
//!
//! The state is what makes an alert one per event: the daemon's poller is the
//! only writer, and it stores the new state and the alert it raises in one
//! transaction, so a restart never repeats an alert and never loses one.

use super::{ChatDb, ChatDbError, query};

/// One `pr_check_state` row: the checks as the last poll found them.
#[derive(Debug, Clone, PartialEq, Eq, skyzen::FromRow)]
pub struct PrCheckState {
    /// The commit the checks ran on.
    pub head_sha: String,
    /// How the checks stood.
    pub rollup: crate::pr::checks::Rollup,
    /// Whether the last poll could not read the checks.
    pub unreadable: bool,
}

#[derive(Debug, serde::Deserialize, skyzen::FromRow)]
struct AlertText {
    text: String,
}

impl ChatDb {
    /// The checks state stored for a PR, when one was ever polled.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn pr_check_state(&self, pr_id: i64) -> Result<Option<PrCheckState>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT head_sha, rollup, unreadable FROM pr_check_state WHERE pr_id = ?",
            pr_id
        )
        .fetch_optional::<PrCheckState>()
        .await?)
    }

    /// Stores `state` for a PR and, in the same transaction, the alert its
    /// change raised.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn store_pr_check_state(
        &self,
        pr_id: i64,
        session_id: &str,
        state: &PrCheckState,
        alert: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let mut tx = self.db.begin().await?;
        query!(
            tx,
            "INSERT INTO pr_check_state (pr_id, head_sha, rollup, unreadable) VALUES (?, ?, ?, ?) ON CONFLICT(pr_id) DO UPDATE SET head_sha = excluded.head_sha, rollup = excluded.rollup, unreadable = excluded.unreadable",
            pr_id,
            state.head_sha.as_str(),
            skyzen_services::ColumnEnum::token(&state.rollup),
            state.unreadable
        )
        .execute()
        .await?;
        if let Some(text) = alert {
            let now = jiff::Timestamp::now().as_millisecond();
            query!(
                tx,
                "INSERT INTO pr_check_alerts (session_id, pr_id, text, created_at_ms) VALUES (?, ?, ?, ?)",
                session_id,
                pr_id,
                text,
                now
            )
            .execute()
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The PR-check alerts the session's agent has not been told, oldest
    /// first, marked as told: each is returned by exactly one call.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn take_pr_check_alerts(&self, session_id: &str) -> Result<Vec<String>, ChatDbError> {
        let mut tx = self.db.begin().await?;
        let rows = query!(
            tx,
            "SELECT text FROM pr_check_alerts WHERE session_id = ? AND alerted = 0 ORDER BY id",
            session_id
        )
        .fetch_all::<AlertText>()
        .await?;
        query!(
            tx,
            "UPDATE pr_check_alerts SET alerted = 1 WHERE session_id = ? AND alerted = 0",
            session_id
        )
        .execute()
        .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|row| row.text).collect())
    }

    /// Every GitHub PR still in flight, across sessions.
    ///
    /// # Errors
    /// Fails on database errors.
    pub async fn live_github_prs(&self) -> Result<Vec<super::pr::PrRow>, ChatDbError> {
        Ok(query!(
            self.db,
            "SELECT * FROM prs WHERE backend = 'github' AND external_number IS NOT NULL AND state IN ('open', 'checks_pending', 'merging')"
        )
        .fetch_all::<super::pr::PrRow>()
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open() -> (tempfile::TempDir, ChatDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = ChatDb::open(&dir.path().join("history.db")).await.unwrap();
        (dir, db)
    }

    use crate::pr::checks::Rollup;

    fn state(head: &str, rollup: Rollup) -> PrCheckState {
        PrCheckState {
            head_sha: head.to_string(),
            rollup,
            unreadable: false,
        }
    }

    #[tokio::test]
    async fn a_stored_alert_is_told_once_and_the_state_is_replaced() {
        let (_dir, db) = open().await;
        assert_eq!(db.pr_check_state(1).await.unwrap(), None);
        db.store_pr_check_state(1, "s", &state("a", Rollup::Failed), Some("测试 failed"))
            .await
            .unwrap();
        db.store_pr_check_state(1, "s", &state("a", Rollup::Failed), None)
            .await
            .unwrap();
        assert_eq!(
            db.pr_check_state(1).await.unwrap(),
            Some(state("a", Rollup::Failed))
        );
        assert_eq!(db.take_pr_check_alerts("s").await.unwrap(), ["测试 failed"]);
        assert_eq!(
            db.take_pr_check_alerts("s").await.unwrap(),
            [] as [String; 0]
        );
        assert_eq!(
            db.take_pr_check_alerts("t").await.unwrap(),
            [] as [String; 0]
        );
    }
}
