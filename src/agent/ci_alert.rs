//! The system alerts a session's agent gets about its PRs' checks: a finished
//! run with an abnormally long job, and a change of the live rollup (a
//! failure, every check passed, the checks unreadable).
//!
//! Each alert is stored when it happens, with its text rendered from its
//! template. This source hands each stored alert to the agent once, at the
//! next request or the end of the tool turn that waited on the checks,
//! whichever comes first.

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId};

use crate::db::{ChatDb, ChatDbError};

const TAG: &str = "ci_alert";

/// Raises the session's stored CI findings as alerts, each once.
#[derive(Debug)]
pub struct CiAlertContext {
    db: ChatDb,
    session_id: String,
}

impl CiAlertContext {
    /// The source for `session_id`.
    #[must_use]
    pub fn new(db: ChatDb, session_id: impl Into<String>) -> Self {
        Self {
            db,
            session_id: session_id.into(),
        }
    }

    async fn take(&self) -> Vec<ContextBlock> {
        let (findings, checks) = futures_lite::future::zip(
            self.db.take_ci_alerts(&self.session_id),
            self.db.take_pr_check_alerts(&self.session_id),
        )
        .await;
        self.block(findings, checks)
    }

    async fn peek_waiting(&self) -> Vec<ContextBlock> {
        let (findings, checks) = futures_lite::future::zip(
            self.db.waiting_ci_alerts(&self.session_id),
            self.db.waiting_pr_check_alerts(&self.session_id),
        )
        .await;
        self.block(findings, checks)
    }

    fn block(
        &self,
        findings: Result<Vec<String>, ChatDbError>,
        checks: Result<Vec<String>, ChatDbError>,
    ) -> Vec<ContextBlock> {
        let mut alerts = Vec::new();
        self.collect(&mut alerts, findings);
        self.collect(&mut alerts, checks);
        if alerts.is_empty() {
            Vec::new()
        } else {
            vec![ContextBlock::stable(TAG, alerts.join("\n\n"))]
        }
    }

    fn collect(&self, alerts: &mut Vec<String>, taken: Result<Vec<String>, ChatDbError>) {
        match taken {
            Ok(taken) => alerts.extend(taken),
            Err(error) => {
                tracing::error!(session_id = %self.session_id, %error, "failed to load CI alerts");
            }
        }
    }
}

impl ContextSource for CiAlertContext {
    fn id(&self) -> SourceId {
        SourceId::new(TAG)
    }

    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } | Moment::ToolTurnEnd => self.take().await,
            _ => Vec::new(),
        }
    }

    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } | Moment::ToolTurnEnd => self.peek_waiting().await,
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_finding_reaches_the_agent_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = ChatDb::open(&dir.path().join("history.db")).await.unwrap();
        let mut source = CiAlertContext::new(db.clone(), "s");
        assert!(source.blocks(Moment::ToolTurnEnd).await.is_empty());
        db.add_ci_finding("s", 1, "abc", "warning", "alert text")
            .await
            .unwrap();
        db.add_ci_finding("other", 2, "def", "warning", "not mine")
            .await
            .unwrap();
        let peeked = source.peek(Moment::ToolTurnEnd).await;
        assert_eq!(peeked.len(), 1, "a preview shows the alert");
        let blocks = source.blocks(Moment::ToolTurnEnd).await;
        let [block] = blocks.as_slice() else {
            panic!("one alert: {blocks:?}");
        };
        assert_eq!(block.block().content(), "alert text");
        assert!(source.blocks(Moment::ToolTurnEnd).await.is_empty());
        assert!(
            source
                .blocks(Moment::Request { reassembled: false })
                .await
                .is_empty()
        );
    }
}
