//! The system alert a session's agent gets when a PR's checks finished with
//! an abnormally long job.
//!
//! The finding is stored when the checks finish, with its text rendered from
//! the alert template. This source hands each stored finding to the agent
//! once, at the next request or the end of the tool turn that waited on the
//! checks, whichever comes first.

use aither_agent::{Block, ContextSource};

use crate::db::ChatDb;

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
        match self.db.take_ci_alerts(&self.session_id).await {
            Ok(alerts) if alerts.is_empty() => None,
            Ok(alerts) => Some(Block::new(TAG, alerts.join("\n\n"))),
            Err(error) => {
                tracing::error!(session_id = %self.session_id, %error, "failed to load CI alerts");
                None
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
