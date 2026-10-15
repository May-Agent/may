use crate::db::ChatDb;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

use super::{
    PendingUserMessage, PendingUserMessageMode, PersistedAskUserRequest,
    PersistedPermissionRequest, PersistedWorkspaceRequest, SessionPlanState, SessionRuntimeState,
    SessionRuntimeSummary, TerminalRuntimeState,
};

/// Persists session runtime state and publishes UI events for changes.
#[derive(Clone)]
pub struct SessionRuntimeService {
    db: ChatDb,
    ui_event_bus: UiEventBus,
}

impl std::fmt::Debug for SessionRuntimeService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionRuntimeService")
            .finish_non_exhaustive()
    }
}

impl SessionRuntimeService {
    /// Create the service over `db`, publishing change events on `ui_event_bus`.
    #[must_use]
    pub const fn new(db: ChatDb, ui_event_bus: UiEventBus) -> Self {
        Self { db, ui_event_bus }
    }

    /// Clone of the underlying database handle.
    #[must_use]
    pub fn db(&self) -> ChatDb {
        self.db.clone()
    }

    fn publish_ui(&self, topics: UiEventTopics) {
        self.ui_event_bus.publish(topics);
    }

    /// Publish a UI invalidation for `topics`.
    pub fn publish_ui_events(&self, topics: UiEventTopics) {
        self.publish_ui(topics);
    }

    /// Load the persisted runtime summary for a session (default when absent).
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure or malformed rows.
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

    /// Persist a session's runtime state.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_state(
        &self,
        session_id: &str,
        state: SessionRuntimeState,
    ) -> Result<(), crate::db::ChatDbError> {
        self.db.set_session_runtime_state(session_id, state).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(())
    }

    /// Persist a session's plan-mode state.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_plan_state(
        &self,
        session_id: &str,
        plan_state: SessionPlanState,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self
            .db
            .set_session_plan_state(session_id, plan_state)
            .await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Append a pending user message to a session's queue.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
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
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Runtime summaries for every session in a profile.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn list_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<(String, SessionRuntimeSummary)>, crate::db::ChatDbError> {
        let sessions = self.db.list_sessions_for_profile(profile_id).await?;
        let session_ids = sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let mut runtime_by_session = self.db.list_session_runtime_summaries(&session_ids).await?;
        let mut runtimes = Vec::with_capacity(sessions.len());
        for session in sessions {
            let session_id = session.id;
            let runtime = runtime_by_session
                .remove(session_id.as_str())
                .unwrap_or_default();
            runtimes.push((session_id, runtime));
        }
        Ok(runtimes)
    }

    /// Pop the next pending message eligible for dispatch.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn take_next_dispatchable(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, crate::db::ChatDbError> {
        let next = self
            .db
            .take_next_dispatchable_pending_message(session_id)
            .await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(next)
    }

    /// Drop the pending immediate message without dispatching it.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn remove_pending_immediate(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.clear_pending_immediate_message(session_id).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Remove the queued message at `index`.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn remove_queued_at(
        &self,
        session_id: &str,
        index: usize,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.remove_queued_message_at(session_id, index).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Record a permission request as blocking the session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_waiting_permission(
        &self,
        session_id: &str,
        request: Option<PersistedPermissionRequest>,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_waiting_permission(session_id, request).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Clear a session's blocking permission request.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn clear_waiting_permission_request(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self
            .db
            .clear_waiting_permission_request(session_id, request_id)
            .await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Record a workspace request as blocking the session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_waiting_workspace(
        &self,
        session_id: &str,
        request: Option<PersistedWorkspaceRequest>,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_waiting_workspace(session_id, request).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Record an ask-user request as blocking the session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_waiting_ask_user(
        &self,
        session_id: &str,
        request: Option<PersistedAskUserRequest>,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_waiting_ask_user(session_id, request).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Atomically transition Running → Idle only if currently Running.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn clear_running_if_needed(
        &self,
        session_id: &str,
    ) -> Result<(), crate::db::ChatDbError> {
        let transitioned = self.db.clear_running_if_idle(session_id).await?;
        if transitioned {
            self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        }
        Ok(())
    }

    /// Mark a peer as owning browser input for the session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.set_browser_takeover_owner(session_id).await?;
        self.publish_ui(
            UiEventTopics::SESSION_RUNTIME
                | UiEventTopics::SESSION_RUNTIMES
                | UiEventTopics::BROWSER_STATUS,
        );
        Ok(summary)
    }

    /// End browser takeover for the session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn clear_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.clear_browser_takeover_owner(session_id).await?;
        self.publish_ui(
            UiEventTopics::SESSION_RUNTIME
                | UiEventTopics::SESSION_RUNTIMES
                | UiEventTopics::BROWSER_STATUS,
        );
        Ok(summary)
    }

    /// Whether a peer currently owns the session's browser input.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn has_browser_takeover(
        &self,
        session_id: &str,
    ) -> Result<bool, crate::db::ChatDbError> {
        self.db.session_has_browser_takeover(session_id).await
    }

    /// Session id currently under browser takeover, if any.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn find_browser_takeover_session(
        &self,
    ) -> Result<Option<String>, crate::db::ChatDbError> {
        self.db.find_browser_takeover_session().await
    }

    /// Consume the takeover-ended alert flag for a session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn take_browser_takeover_alert_pending(
        &self,
        session_id: &str,
    ) -> Result<bool, crate::db::ChatDbError> {
        let pending = self
            .db
            .take_browser_takeover_alert_pending(session_id)
            .await?;
        if pending {
            self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        }
        Ok(pending)
    }

    /// Persist terminal runtime state for a session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_terminal_runtime(
        &self,
        session_id: &str,
        terminal: TerminalRuntimeState,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        self.db
            .set_terminal_interaction(session_id, &terminal)
            .await?;
        let summary = self.get(session_id).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Clear a session's terminal runtime state.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn clear_terminal_runtime(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        self.set_terminal_runtime(session_id, TerminalRuntimeState::default())
            .await
    }
}
