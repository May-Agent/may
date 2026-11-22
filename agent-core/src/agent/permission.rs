//! Permission approval for terminal tool escalations.
//!
//! `PermissionApprover` is the [`aither_core::llm::Approver`] used by
//! `MayAgent`. It binds to a session, routes escalation requests through the
//! runtime service so they are persisted and visible in the UI, and blocks
//! the call until the user responds (or the request is interrupted).

pub use may_protocol::permission::{
    PermissionHistoryMode, PermissionRequest, PermissionRequestMode,
};
use std::sync::Arc;

use aither_core::llm::{Approval, ApprovalRequest, Approver, ApproverFailure};
use aither_sandbox::SandboxMode;

use crate::live_response::LiveResponseRegistry;

/// The [`aither_core::llm::ApprovalKind`] of a script that runs outside the
/// sandbox.
pub const UNSANDBOXED_KIND: &str = "unsandboxed";

/// Why a permission request did not end in an approval.
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    /// The request was denied.
    #[error("{0}")]
    Denied(String),
    /// The run was interrupted while the request was pending.
    #[error("the permission request was interrupted")]
    Interrupted,
    /// The request could not be recorded or read.
    #[error(transparent)]
    Internal(anyhow::Error),
}

/// How a script's mode is phrased in a sentence about it.
const fn describe(mode: SandboxMode) -> &'static str {
    mode.description()
}

/// How long an escalation from the main agent stays pending before it
/// resolves as denied. The operator is not assumed to be at the screen.
pub const MAIN_APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(30);

/// The same deadline for escalations raised by a subagent: they should
/// not hold a run nearly as long.
pub const SUBAGENT_APPROVAL_TIMEOUT: std::time::Duration = std::time::Duration::from_mins(3);

/// Scope of an in-flight permission escalation request.
/// What an approval request nobody answers resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnTimeout {
    /// Denied, as an `unsafe` escalation is.
    Deny,
    /// Approved.
    Approve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalMode {
    Interactive,
    Unavailable { context: &'static str },
}

/// Host-side permission coordinator bound to a session's runtime service.
#[derive(Debug, Clone)]
pub struct PermissionApprover {
    live_responses: Arc<LiveResponseRegistry<bool>>,
    session_id: Option<String>,
    /// The agent this approver speaks for: every agent in an instance — the
    /// session's own agent and each of its subagents — gets a distinct
    /// `word-word-word` id so requests stay attributable and addressable.
    agent_id: String,
    /// The session's runtime service, wired where the daemon runs.
    pub runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    approval_mode: ApprovalMode,
    timeout: std::time::Duration,
    /// The interpreter of each `terminal` call's script, so a request's
    /// prompt highlights the script in its language.
    interpreters: crate::agent::hooks::CallInterpreters,
    /// Holds a command's timeout clock open while a prompt of this approver
    /// waits on the user.
    approval_pause: crate::harness::approval_pause::ApprovalPause,
    /// One prompt at a time: a second request waits for the first's answer
    /// rather than replacing the row the user is deciding on.
    prompt_slot: Arc<async_lock::Mutex<()>>,
}

impl PermissionApprover {
    /// Create an approver with no session binding.
    #[must_use]
    pub fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            session_id: None,
            agent_id: crate::id::new(),
            runtime_service: None,
            approval_mode: ApprovalMode::Interactive,
            timeout: MAIN_APPROVAL_TIMEOUT,
            interpreters: crate::agent::hooks::CallInterpreters::default(),
            approval_pause: crate::harness::approval_pause::ApprovalPause::new(),
            prompt_slot: Arc::new(async_lock::Mutex::new(())),
        }
    }

    /// Return a copy bound to `session_id`, speaking for a fresh agent id:
    /// each session's main agent is its own agent.
    #[must_use]
    pub fn for_session(&self, session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        let agent_id = crate::id::new();
        if let Some(runtime_service) = &self.runtime_service {
            runtime_service.register_agent(&agent_id, &session_id);
        }
        Self {
            live_responses: self.live_responses.clone(),
            session_id: Some(session_id),
            agent_id,
            runtime_service: self.runtime_service.clone(),
            approval_mode: self.approval_mode,
            timeout: self.timeout,
            interpreters: self.interpreters.clone(),
            approval_pause: self.approval_pause.clone(),
            prompt_slot: self.prompt_slot.clone(),
        }
    }

    /// The pause the approver holds its prompts on: commands awaiting the
    /// answer must not count the wait against their own timeout.
    #[must_use]
    pub fn approval_pause(&self) -> crate::harness::approval_pause::ApprovalPause {
        self.approval_pause.clone()
    }

    /// Holds the prompts of this approver on `pause`, shared with the other
    /// waits the session's command timeouts honor.
    #[must_use]
    pub fn with_approval_pause(
        mut self,
        pause: crate::harness::approval_pause::ApprovalPause,
    ) -> Self {
        self.approval_pause = pause;
        self
    }

    /// Return a copy whose pending requests auto-deny after `timeout`.
    #[must_use]
    pub const fn with_approval_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The approval deadline and fresh agent id for requests raised by a
    /// subagent, registered against the parent's session.
    #[must_use]
    pub fn for_subagent(mut self) -> Self {
        self.agent_id = crate::id::new();
        if let (Some(runtime_service), Some(session_id)) = (&self.runtime_service, &self.session_id)
        {
            runtime_service.register_agent(&self.agent_id, session_id);
        }
        self.with_approval_timeout(SUBAGENT_APPROVAL_TIMEOUT)
    }

    /// The agent this approver's requests are attributed to.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Attach the runtime service used to persist/surface requests.
    #[must_use]
    pub fn with_runtime_service(
        mut self,
        runtime_service: Arc<crate::session::SessionRuntimeService>,
    ) -> Self {
        self.runtime_service = Some(runtime_service);
        self
    }

    /// Share the record of each `terminal` call's interpreter, so a request
    /// for a call shows its script in that call's language.
    #[must_use]
    pub fn with_interpreters(
        mut self,
        interpreters: crate::agent::hooks::CallInterpreters,
    ) -> Self {
        self.interpreters = interpreters;
        self
    }

    /// The attached runtime service, if any.
    #[must_use]
    pub fn runtime_service(&self) -> Option<Arc<crate::session::SessionRuntimeService>> {
        self.runtime_service.clone()
    }

    /// Session id this approver is bound to, if any.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Mark this approver as unable to prompt interactively, with a reason.
    #[must_use]
    pub const fn without_interactive_approval(mut self, context: &'static str) -> Self {
        self.approval_mode = ApprovalMode::Unavailable { context };
        self
    }

    fn unavailable_error(mode: SandboxMode, context: &'static str) -> PermissionError {
        PermissionError::Denied(format!(
            "sandbox blocked {} execution; approval is required to escalate this command, but approval is unavailable in {context}",
            mode.description()
        ))
    }

    fn persisted_request(
        request_id: String,
        payload: PermissionRequest,
    ) -> crate::session::PersistedPermissionRequest {
        crate::session::PersistedPermissionRequest {
            id: request_id,
            request: crate::session::PersistedPermissionRequestPayload {
                mode: payload.mode,
                script: payload.script,
                session_id: payload.session_id,
                agent_id: payload.agent_id,
                reading: payload.reading,
                interpreter: payload.interpreter,
            },
        }
    }

    async fn append_request_history(
        db: &crate::db::ChatDb,
        session_id: &str,
        request_id: &str,
        mode: PermissionHistoryMode,
        script: &str,
        reading: Option<&crate::command_reading::CommandReading>,
    ) -> Result<(), PermissionError> {
        db.append_permission_request_message(session_id, request_id, mode, script, reading, None)
            .await
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)?;
        Ok(())
    }

    /// Whether the session's yolo switch is on; read per request so a
    /// change applies at once.
    async fn yolo(&self) -> Result<bool, PermissionError> {
        let (Some(runtime_service), Some(session_id)) = (&self.runtime_service, &self.session_id)
        else {
            return Ok(false);
        };
        runtime_service
            .db()
            .session_yolo(session_id)
            .await
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)
    }

    /// Record a yolo grant in the session history like a user approval.
    async fn record_yolo_grant(
        &self,
        mode: PermissionHistoryMode,
        script: &str,
    ) -> Result<(), PermissionError> {
        let (Some(runtime_service), Some(session_id)) = (&self.runtime_service, &self.session_id)
        else {
            return Ok(());
        };
        runtime_service
            .db()
            .append_granted_permission(
                session_id,
                mode,
                script,
                (mode != PermissionHistoryMode::Publish)
                    .then(|| crate::command_reading::CommandReading::of(script))
                    .as_ref(),
            )
            .await
            .map_err(anyhow::Error::from)
            .map_err(PermissionError::Internal)?;
        Ok(())
    }

    /// Ask the user for permission and wait for a decision.
    ///
    /// # Errors
    /// Returns [`PermissionError`] when the request cannot be delivered or the wait is interrupted.
    pub async fn request_and_wait(
        &self,
        mode: SandboxMode,
        script: String,
        interpreter: Option<aither_sandbox::Interpreter>,
    ) -> Result<bool, PermissionError> {
        self.request_and_wait_for_call(mode, script, interpreter, None)
            .await
    }

    /// [`Self::request_and_wait`] attributing the timeout hold to `call`:
    /// the tool call the request came from; `None` holds the whole session.
    ///
    /// # Errors
    /// Returns [`PermissionError`] when the request cannot be delivered or the wait is interrupted.
    pub async fn request_and_wait_for_call(
        &self,
        mode: SandboxMode,
        script: String,
        interpreter: Option<aither_sandbox::Interpreter>,
        call: Option<aither_core::llm::ToolCallId>,
    ) -> Result<bool, PermissionError> {
        if self.yolo().await? {
            self.record_yolo_grant(mode.into(), &script).await?;
            return Ok(true);
        }
        if let ApprovalMode::Unavailable { context } = self.approval_mode {
            return Err(Self::unavailable_error(mode, context));
        }
        self.wait(
            mode.into(),
            mode.into(),
            &format!("{} mode", mode.description()),
            script,
            interpreter,
            call,
            OnTimeout::Deny,
        )
        .await
    }

    /// Ask the user to approve publishing `text` (an issue, pull request or
    /// comment) and wait for the decision. Where an unanswered `unsafe`
    /// request is denied, `on_timeout` decides: [`OnTimeout::Approve`]
    /// approves it. A yolo session approves at once; an approver that
    /// cannot prompt behaves as if nobody answered.
    ///
    /// # Errors
    /// Returns [`PermissionError`] when the request cannot be delivered, the
    /// wait is interrupted, or an unanswered request is denied.
    pub async fn request_publication(
        &self,
        text: String,
        on_timeout: OnTimeout,
    ) -> Result<bool, PermissionError> {
        self.request_publication_for_call(text, on_timeout, None)
            .await
    }

    /// [`Self::request_publication`] attributing the timeout hold to `call`.
    ///
    /// # Errors
    /// Returns [`PermissionError`] when the request cannot be delivered, the
    /// wait is interrupted, or an unanswered request is denied.
    pub async fn request_publication_for_call(
        &self,
        text: String,
        on_timeout: OnTimeout,
        call: Option<aither_core::llm::ToolCallId>,
    ) -> Result<bool, PermissionError> {
        if self.yolo().await? {
            self.record_yolo_grant(PermissionHistoryMode::Publish, &text)
                .await?;
            return Ok(true);
        }
        if let ApprovalMode::Unavailable { context } = self.approval_mode {
            return match on_timeout {
                OnTimeout::Approve => Ok(true),
                OnTimeout::Deny => Err(PermissionError::Denied(format!(
                    "publishing needs the user's approval, which is unavailable in {context}"
                ))),
            };
        }
        self.wait(
            PermissionRequestMode::Publish,
            PermissionHistoryMode::Publish,
            "publishing",
            text,
            None,
            call,
            on_timeout,
        )
        .await
    }

    /// Persist the request, surface it to the UI and wait for the answer.
    ///
    /// `call` attributes the timeout hold to the tool call the request came
    /// from; `None` holds the whole session while the prompt is open.
    #[allow(
        clippy::too_many_arguments,
        reason = "the request's parts are passed as they are"
    )]
    async fn wait(
        &self,
        mode: PermissionRequestMode,
        history: PermissionHistoryMode,
        what: &str,
        script: String,
        interpreter: Option<aither_sandbox::Interpreter>,
        call: Option<aither_core::llm::ToolCallId>,
        on_timeout: OnTimeout,
    ) -> Result<bool, PermissionError> {
        enum Outcome {
            Answered(Result<bool, async_channel::RecvError>),
            Expired,
        }
        // The user is being asked: whatever command waits on this answer
        // must not count the wait against its own timeout.
        let _pause = self.approval_pause.hold(call);
        let Some(session_id) = self.session_id.clone() else {
            return Err(PermissionError::Internal(anyhow::anyhow!(
                "permission approval requires a bound session id"
            )));
        };

        let reading = (mode != PermissionRequestMode::Publish)
            .then(|| crate::command_reading::CommandReading::of(&script));
        let payload = PermissionRequest {
            mode,
            script,
            session_id: Some(session_id.clone()),
            agent_id: Some(self.agent_id.clone()),
            reading,
            interpreter,
        };
        let request_id = crate::id::new();
        let rx = self.live_responses.open(&request_id);

        // One prompt at a time: the storage a request lands in is
        // session-wide, so a second prompt is queued rather than replacing
        // the one the user is deciding on.
        let _slot = self.prompt_slot.lock().await;
        // Drops before `_slot`: the request row is cleared first, then the
        // next queued prompt surfaces.
        let _pending = PendingPrompt {
            request_id: request_id.clone(),
            live_responses: Arc::clone(&self.live_responses),
            runtime_service: self.runtime_service.clone(),
            session_id: session_id.clone(),
        };

        if let Some(runtime_service) = self.runtime_service.clone() {
            runtime_service
                .set_waiting_permission(
                    &session_id,
                    Some(Self::persisted_request(request_id.clone(), payload.clone())),
                )
                .await
                .map_err(anyhow::Error::from)
                .map_err(PermissionError::Internal)?;
            let db = runtime_service.db();
            Self::append_request_history(
                &db,
                &session_id,
                &request_id,
                history,
                &payload.script,
                payload.reading.as_ref(),
            )
            .await?;
            runtime_service.announce_user_wait(crate::session::UserWait {
                session_id: session_id.clone(),
                ask: crate::session::UserAsk::permission(mode, &payload.script),
            });
        }

        let outcome =
            futures_lite::future::race(async { Outcome::Answered(rx.recv().await) }, async {
                async_io::Timer::after(self.timeout).await;
                Outcome::Expired
            })
            .await;
        if let Outcome::Answered(decision) = outcome {
            return decision.map_err(|_| PermissionError::Interrupted);
        }
        // The operator never answered: resolve the pending request as
        // the timeout outcome so it stops blocking the UI and the history
        // records the decision. The waiter is this run itself — there is
        // nothing to wake.
        let approved = on_timeout == OnTimeout::Approve;
        self.live_responses.take(&request_id);
        if let Some(runtime_service) = &self.runtime_service
            && let Err(error) = runtime_service
                .commit_permission_response(&request_id, approved)
                .await
        {
            tracing::warn!(%error, "failed to persist a timed-out permission decision");
        }
        if approved {
            return Ok(true);
        }
        Err(PermissionError::Denied(format!(
            "{what} approval was not answered within {} minutes and was denied",
            self.timeout.as_secs() / 60
        )))
    }

    /// Resolve the pending request `request_id`: the decision is committed
    /// first (the request cleared by its id, the decision in the history),
    /// and only then is the waiting run woken. Returns `None` when the
    /// request is not pending.
    ///
    /// # Errors
    /// Returns [`crate::db::ChatDbError`] when the commit fails; nothing is
    /// woken then.
    ///
    /// # Panics
    /// Panics if the approver has no runtime service.
    pub async fn resolve(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<Option<crate::session::ResolvedRequest>, crate::db::ChatDbError> {
        let runtime = self
            .runtime_service
            .as_ref()
            .expect("resolving a permission request needs the runtime service");
        let Some(session_id) = runtime
            .commit_permission_response(request_id, approved)
            .await?
        else {
            return Ok(None);
        };
        let woke = self.respond(request_id, approved);
        Ok(Some(crate::session::ResolvedRequest { session_id, woke }))
    }

    /// The requests awaiting an answer.
    #[cfg(test)]
    pub(crate) fn pending_request_ids(&self) -> Vec<String> {
        self.live_responses.pending_ids()
    }

    /// Deliver a decision for `request_id`; returns false if no matching pending request exists.
    #[must_use]
    pub fn respond(&self, request_id: &str, approved: bool) -> bool {
        self.live_responses.resolve(request_id, approved)
    }
}

impl Default for PermissionApprover {
    fn default() -> Self {
        Self::new()
    }
}

/// A pending permission prompt: on drop — an answer, a timeout or a
/// cancelled wait — the live response slot frees and the request row clears,
/// so nothing waits on a prompt nobody will see.
struct PendingPrompt {
    request_id: String,
    live_responses: Arc<LiveResponseRegistry<bool>>,
    runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    session_id: String,
}

impl Drop for PendingPrompt {
    fn drop(&mut self) {
        self.live_responses.take(&self.request_id);
        let Some(runtime_service) = self.runtime_service.clone() else {
            return;
        };
        let session_id = self.session_id.clone();
        let request_id = self.request_id.clone();
        let clear = async move {
            if let Err(error) = runtime_service
                .clear_waiting_permission_request(&session_id, &request_id)
                .await
            {
                tracing::warn!(%error, "failed to clear a dropped permission request");
            }
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(clear);
        }
    }
}

impl Approver for PermissionApprover {
    /// Asks the user about a script that runs outside the sandbox. Every
    /// other kind of request reaches the approver only through a surface
    /// that answers it itself.
    async fn approve(&self, request: ApprovalRequest) -> Result<Approval, ApproverFailure> {
        assert_eq!(
            request.kind().as_str(),
            UNSANDBOXED_KIND,
            "the permission approver answers scripts that run outside the sandbox"
        );
        let interpreter = request.call().and_then(|call| self.interpreters.get(call));
        let call = request.call().map(|identity| identity.call().clone());
        match self
            .request_and_wait_for_call(
                SandboxMode::Unsandboxed,
                request.subject().to_string(),
                interpreter,
                call,
            )
            .await
        {
            Ok(true) => Ok(Approval::Approved),
            Ok(false) => Ok(Approval::Denied {
                reason: format!(
                    "{} mode denied for script",
                    describe(SandboxMode::Unsandboxed)
                ),
            }),
            Err(PermissionError::Denied(reason)) => Ok(Approval::Denied { reason }),
            Err(error) => Err(ApproverFailure::new(error)),
        }
    }
}

impl PermissionApprover {
    /// Whether the user would be asked about an unsandboxed script now:
    /// interactive, and the session's yolo switch is off.
    pub async fn will_wait_for_approval(&self) -> bool {
        !matches!(self.approval_mode, ApprovalMode::Unavailable { .. })
            && !self.yolo().await.unwrap_or_else(|error| {
                tracing::warn!(%error, "failed to read the session yolo switch");
                false
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_timeout_is_thirty_minutes() {
        assert_eq!(PermissionApprover::new().timeout, MAIN_APPROVAL_TIMEOUT);
    }

    #[test]
    fn subagent_approver_uses_the_shorter_deadline() {
        assert_eq!(
            PermissionApprover::new().for_subagent().timeout,
            SUBAGENT_APPROVAL_TIMEOUT
        );
        assert!(SUBAGENT_APPROVAL_TIMEOUT < MAIN_APPROVAL_TIMEOUT);
    }

    #[test]
    fn every_agent_gets_a_distinct_id() {
        let base = PermissionApprover::new();
        let main_a = base.for_session("a");
        let main_b = base.for_session("b");
        let sub = main_a.clone().for_subagent();
        for (left, right) in [
            (main_a.agent_id(), main_b.agent_id()),
            (main_a.agent_id(), sub.agent_id()),
            (base.agent_id(), main_a.agent_id()),
        ] {
            assert_ne!(left, right, "agents must not share an id");
            assert_eq!(
                left.matches('-').count(),
                2,
                "agent id must be word-word-word: {left}"
            );
        }
    }

    #[test]
    fn for_session_preserves_the_timeout() {
        let approver = PermissionApprover::new()
            .with_approval_timeout(std::time::Duration::from_secs(5))
            .for_session("session-1");
        assert_eq!(approver.timeout, std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn an_unanswered_request_times_out_denied() {
        let approver = PermissionApprover::new()
            .with_approval_timeout(std::time::Duration::from_millis(50))
            .for_session("session-1");
        let error = approver
            .request_and_wait(SandboxMode::Unsandboxed, "cargo build".to_string(), None)
            .await
            .expect_err("an unanswered request must not succeed");
        let PermissionError::Denied(message) = error else {
            panic!("expected a denial, got {error:?}");
        };
        assert!(message.contains("denied"), "unexpected message: {message}");
    }

    #[tokio::test]
    async fn an_answered_request_returns_the_decision() {
        let approver = PermissionApprover::new()
            .with_approval_timeout(std::time::Duration::from_secs(30))
            .for_session("session-1");
        let responder = approver.clone();
        let wait = tokio::spawn(async move {
            approver
                .request_and_wait(SandboxMode::Unsandboxed, "cargo build".to_string(), None)
                .await
        });
        // The request id is internal; approve whatever is pending.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let pending = responder.live_responses.pending_ids();
        assert_eq!(pending.len(), 1);
        assert!(responder.respond(&pending[0], true));
        assert!(wait.await.expect("join").expect("answered"));
    }
}
