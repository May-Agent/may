use crate::db::ChatDb;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

use super::{
    AcquireError, PendingUserMessage, PendingUserMessageMode, PersistedAskUserRequest,
    PersistedPermissionRequest, PersistedWorkspaceRequest, RunOutcome, SessionPlanState,
    SessionRuns, SessionRuntimeState, SessionRuntimeSummary, TerminalRuntimeState,
};

/// How often a stop re-checks whether the stopped run has ended.
const STOP_SETTLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(120);

/// A pending request that a response resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRequest {
    /// Session the request belonged to.
    pub session_id: String,
    /// Whether a live waiter was woken with the response. When not, nothing
    /// is waiting: the caller resumes the session.
    pub woke: bool,
}

/// Why an answer was not committed.
#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    /// The answers do not fit the question.
    #[error(transparent)]
    Mismatch(#[from] super::open_questions::AnswerMismatch),
    /// The commit failed; nothing was written.
    #[error("failed to commit the answer: {0}")]
    Storage(#[from] crate::db::ChatDbError),
}

/// What answering a question found.
#[derive(Debug)]
pub enum AskUserAnswer {
    /// The answer was committed by this call.
    Committed(crate::db::CommittedAskUserAnswer),
    /// An earlier call committed an answer already.
    AlreadyAnswered {
        /// Session the question belonged to.
        session_id: String,
    },
}

/// Persists session runtime state and publishes UI events for changes.
#[derive(Clone)]
pub struct SessionRuntimeService {
    db: ChatDb,
    ui_event_bus: UiEventBus,
    runs: SessionRuns,
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
    pub fn new(db: ChatDb, ui_event_bus: UiEventBus) -> Self {
        Self::with_run_ttl(db, ui_event_bus, super::DEFAULT_RUN_LEASE_TTL)
    }

    /// Create the service with run leases that expire after `ttl` without a
    /// heartbeat.
    #[must_use]
    pub fn with_run_ttl(db: ChatDb, ui_event_bus: UiEventBus, ttl: std::time::Duration) -> Self {
        let runs = SessionRuns::new(db.clone(), ui_event_bus.clone()).with_ttl(ttl);
        Self {
            db,
            ui_event_bus,
            runs,
        }
    }

    /// Spawn the background tasks of run leases with `spawner`; see
    /// [`super::run_lease::Spawner`].
    #[must_use]
    pub fn with_spawner(mut self, spawner: super::run_lease::Spawner) -> Self {
        self.runs = self.runs.with_spawner(spawner);
        self
    }

    /// Single-owner run leases of this database.
    #[must_use]
    pub const fn runs(&self) -> &SessionRuns {
        &self.runs
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

    /// Persist a session's runtime state as the run holding `fence`.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError::RunLeaseLost`] when the run no longer
    /// holds the lease, or [`crate::db::ChatDbError`] on storage failure.
    pub async fn set_state_as_run(
        &self,
        fence: &crate::db::RunFence,
        state: SessionRuntimeState,
    ) -> Result<(), crate::db::ChatDbError> {
        self.db
            .set_session_runtime_state_as_run(fence, state)
            .await?;
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

    /// Hold until the stopped session has no live run, then settle whatever
    /// state it was left in. A session still waiting on a request after its
    /// run ended (a restart dropped the run that was waiting) has no run to
    /// settle it, so this takes the session's run itself and finishes it.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn settle_stopped(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        loop {
            let runtime = self.get(session_id).await?;
            if !self.runs.is_held(session_id).await? {
                if !runtime.state.is_active() || runtime.interactions.is_active() {
                    return Ok(runtime);
                }
                match self.runs.acquire(session_id).await {
                    Ok(guard) => {
                        guard.finish_idle(RunOutcome::Cancelled).await?;
                        return self.get(session_id).await;
                    }
                    Err(AcquireError::Busy(_)) => {}
                    Err(AcquireError::Storage(error)) => return Err(error),
                }
            }
            async_io::Timer::after(STOP_SETTLE_POLL_INTERVAL).await;
        }
    }

    /// Pop the next pending message for the run holding the session's lease.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn take_next_for_run(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, crate::db::ChatDbError> {
        let next = self
            .db
            .take_next_pending_message_for_run(session_id)
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

    /// Drop every pending message for a session.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn clear_pending_messages(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        let summary = self.db.clear_pending_messages(session_id).await?;
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

    /// Clear the ask-user prompt `request_id` by its id; a newer prompt of the
    /// session stays.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn clear_waiting_ask_user_request(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        self.db.clear_waiting_ask_user_request(request_id).await?;
        let summary = self.get(session_id).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(summary)
    }

    /// Persist an open question: the agent asked it and no run waits on it.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn open_question(
        &self,
        session_id: &str,
        request: &PersistedAskUserRequest,
    ) -> Result<(), crate::db::ChatDbError> {
        self.db.open_ask_user_question(session_id, request).await?;
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(())
    }

    /// Withdraw an open question. Returns whether it was still open.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn withdraw_question(
        &self,
        session_id: &str,
        request_id: &str,
    ) -> Result<bool, crate::db::ChatDbError> {
        let withdrawn = self
            .db
            .withdraw_ask_user_question(session_id, request_id)
            .await?;
        if withdrawn {
            self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        }
        Ok(withdrawn)
    }

    /// Commit the answer to the pending question `request_id`.
    ///
    /// An open question (no run waits on it) has the answer join the session
    /// queue in the same transaction, as a steer message carrying it in full;
    /// the caller then starts the dispatch. A prompt a run blocks on is an
    /// internal decision, not a question the agent asked: nothing is queued,
    /// and the caller wakes the waiter or resumes the session. A question already answered reports
    /// [`AskUserAnswer::AlreadyAnswered`], so a retry of a committed answer
    /// finds the state it committed; `None` means the question is not
    /// pending and was never answered (withdrawn or never asked): nothing is
    /// delivered.
    ///
    /// # Errors
    /// Returns [`AnswerError`] when the answers do not fit the question or on
    /// storage failure; nothing is committed then.
    pub async fn answer_ask_user(
        &self,
        request_id: &str,
        answers: &[crate::ask_user::QuestionAnswer],
    ) -> Result<Option<AskUserAnswer>, AnswerError> {
        let Some(pending) = self.db.pending_ask_user_request(request_id).await? else {
            return Ok(self
                .db
                .answered_ask_user_session(request_id)
                .await?
                .map(|session_id| AskUserAnswer::AlreadyAnswered { session_id }));
        };
        super::open_questions::check_answers(&pending.request, answers)?;
        let committed = self
            .db
            .commit_ask_user_answer(
                request_id,
                answers,
                (!pending.blocking).then_some(|request: &PersistedAskUserRequest| {
                    super::open_questions::answer_delivery(request, answers)
                }),
            )
            .await?;
        let Some(committed) = committed else {
            // Answered or withdrawn between the lookup and the commit.
            return Ok(self
                .db
                .answered_ask_user_session(request_id)
                .await?
                .map(|session_id| AskUserAnswer::AlreadyAnswered { session_id }));
        };
        self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        Ok(Some(AskUserAnswer::Committed(committed)))
    }

    /// Queue the delivery of an answer whose live waiter was gone by the time
    /// it was woken.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure.
    pub async fn deliver_ask_user_answer(
        &self,
        request: &PersistedAskUserRequest,
        session_id: &str,
        answers: &[crate::ask_user::QuestionAnswer],
    ) -> Result<SessionRuntimeSummary, crate::db::ChatDbError> {
        self.enqueue(
            session_id,
            super::open_questions::answer_delivery(request, answers),
            PendingUserMessageMode::Steer,
        )
        .await
    }

    /// Commit a permission decision, clearing the pending request by its id.
    /// Returns the request's session, or `None` when it is not pending.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure; nothing is
    /// committed then.
    pub async fn commit_permission_response(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<String>, crate::db::ChatDbError> {
        let session_id = self
            .db
            .commit_permission_response(request_id, approved)
            .await?;
        if session_id.is_some() {
            self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        }
        Ok(session_id)
    }

    /// Commit a workspace decision, clearing the pending request by its id.
    /// Returns the request's session, or `None` when it is not pending.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] on storage failure; nothing is
    /// committed then.
    pub async fn commit_workspace_response(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<String>, crate::db::ChatDbError> {
        let session_id = self
            .db
            .commit_workspace_response(request_id, approved)
            .await?;
        if session_id.is_some() {
            self.publish_ui(UiEventTopics::SESSION_RUNTIME | UiEventTopics::SESSION_RUNTIMES);
        }
        Ok(session_id)
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
