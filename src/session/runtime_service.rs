use std::sync::Arc;

use tokio::sync::Notify;

use crate::db::ChatDb;

use super::{
    PendingUserMessage, PendingUserMessageMode, PersistedAskUserRequest,
    PersistedPermissionRequest, PersistedWorkspaceRequest, SessionPlanState, SessionRuntimeState,
    SessionRuntimeSummary, TerminalRuntimeState,
};

#[derive(Clone)]
pub struct SessionRuntimeService {
    db: ChatDb,
    notify: Arc<Notify>,
}

impl std::fmt::Debug for SessionRuntimeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuntimeService")
            .finish_non_exhaustive()
    }
}

impl SessionRuntimeService {
    pub const fn new(db: ChatDb, notify: Arc<Notify>) -> Self {
        Self { db, notify }
    }

    #[must_use] 
    pub fn db(&self) -> ChatDb {
        self.db.clone()
    }

    fn notify_ui(&self) {
        self.notify.notify_waiters();
    }

    pub async fn get(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        Ok(self
            .db
            .get_session_runtime(session_id)
            .await?
            .unwrap_or_default())
    }

    pub async fn set_state(
        &self,
        session_id: &str,
        state: SessionRuntimeState,
    ) -> Result<(), crate::db::ChatDbError> {
        self.db.set_session_runtime_state(session_id, state).await?;
        self.notify_ui();
        Ok(())
    }

    pub async fn set_plan_state(
        &self,
        session_id: &str,
        plan_state: SessionPlanState,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self
            .db
            .set_session_plan_state(session_id, plan_state)
            .await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn enqueue(
        &self,
        session_id: &str,
        message: PendingUserMessage,
        mode: PendingUserMessageMode,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = if mode == PendingUserMessageMode::Queue {
            self.db.push_queued_message(session_id, message).await?
        } else {
            self.db
                .set_pending_immediate_message(session_id, message)
                .await?
        };
        self.notify_ui();
        Ok(summary)
    }

    pub async fn list_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<(String, SessionRuntimeSummary)>, crate::db::ChatDbError> {
        let sessions = self.db.list_sessions_for_profile(profile_id).await?;
        let mut runtimes = Vec::with_capacity(sessions.len());
        for session in sessions {
            let session_id = session.id;
            let runtime = self
                .db
                .get_session_runtime(&session_id)
                .await?
                .unwrap_or_default();
            runtimes.push((session_id, runtime));
        }
        Ok(runtimes)
    }

    pub async fn take_next_dispatchable(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, crate::db::ChatDbError> {
        let next = self
            .db
            .take_next_dispatchable_pending_message(session_id)
            .await?;
        self.notify_ui();
        Ok(next)
    }

    pub async fn remove_pending_immediate(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.clear_pending_immediate_message(session_id).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn remove_queued_at(
        &self,
        session_id: &str,
        index: usize,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.remove_queued_message_at(session_id, index).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn set_waiting_permission(
        &self,
        session_id: &str,
        request: Option<PersistedPermissionRequest>,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_waiting_permission(session_id, request).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn set_waiting_workspace(
        &self,
        session_id: &str,
        request: Option<PersistedWorkspaceRequest>,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_waiting_workspace(session_id, request).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn set_waiting_ask_user(
        &self,
        session_id: &str,
        request: Option<PersistedAskUserRequest>,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_waiting_ask_user(session_id, request).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn clear_running_if_needed(
        &self,
        session_id: &str,
    ) -> Result<(), crate::db::ChatDbError> {
        let runtime = self.get(session_id).await?;
        if runtime.state == SessionRuntimeState::Running {
            self.set_state(session_id, SessionRuntimeState::Idle)
                .await?;
        }
        Ok(())
    }

    pub async fn set_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_browser_takeover_owner(session_id).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn clear_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.clear_browser_takeover_owner(session_id).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn has_browser_takeover(
        &self,
        session_id: &str,
    ) -> Result<bool, crate::db::ChatDbError> {
        self.db.session_has_browser_takeover(session_id).await
    }

    pub async fn find_browser_takeover_session(
        &self,
    ) -> Result<Option<String>, crate::db::ChatDbError> {
        self.db.find_browser_takeover_session().await
    }

    pub async fn take_browser_takeover_alert_pending(
        &self,
        session_id: &str,
    ) -> Result<bool, crate::db::ChatDbError> {
        let pending = self
            .db
            .take_browser_takeover_alert_pending(session_id)
            .await?;
        if pending {
            self.notify_ui();
        }
        Ok(pending)
    }

    pub async fn set_terminal_runtime(
        &self,
        session_id: &str,
        terminal: TerminalRuntimeState,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let mut summary = self.get(session_id).await?;
        summary.interactions.terminal = terminal;
        summary.normalize_state();
        self.db.write_session_runtime(session_id, &summary).await?;
        self.notify_ui();
        Ok(summary)
    }

    pub async fn clear_terminal_runtime(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        self.set_terminal_runtime(session_id, TerminalRuntimeState::default())
            .await
    }
}
