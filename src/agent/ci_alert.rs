//! The system alerts a session's agent gets about its PRs' checks: a finished
//! run with an abnormally long job, and a change of the live rollup (a
//! failure, every check passed, the checks unreadable).
//!
//! Each alert is stored when it happens, with its text rendered from its
//! template. This source hands each stored alert to the agent once, at the
//! next request or the end of the tool turn that waited on the checks,
//! whichever comes first.

use aither_agent::{Block, ContextSource};

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

    async fn take(&self) -> Option<Block> {
        let (findings, checks) = futures_lite::future::zip(
            self.db.take_ci_alerts(&self.session_id),
            self.db.take_pr_check_alerts(&self.session_id),
        )
        .await;
        let mut alerts = Vec::new();
        self.collect(&mut alerts, findings);
        self.collect(&mut alerts, checks);
        (!alerts.is_empty()).then(|| Block::new(TAG, alerts.join("\n\n")))
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
    async fn on_request(&mut self, _reassembled: bool) -> Option<Block> {
        self.take().await
    }

    async fn on_tool_turn_end(&mut self) -> Option<Block> {
        self.take().await
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
        assert!(source.on_tool_turn_end().await.is_none());
        db.add_ci_finding("s", 1, "abc", "warning", "alert text")
            .await
            .unwrap();
        db.add_ci_finding("other", 2, "def", "warning", "not mine")
            .await
            .unwrap();
        let block = source.on_tool_turn_end().await.expect("one alert");
        assert_eq!(block.content(), "alert text");
        assert!(source.on_tool_turn_end().await.is_none());
        assert!(source.on_request(false).await.is_none());
    }
}
