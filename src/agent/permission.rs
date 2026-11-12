//! Permission approval for terminal tool escalations.
//!
//! `PermissionApprover` is the [`aither_core::llm::Approver`] used by
//! `MayAgent`. It binds to a session, routes escalation requests through the
//! runtime service so they are persisted and visible in the UI, and blocks
//! the call until the user responds (or the request is interrupted).

use std::sync::Arc;

use aither_core::llm::{Approval, ApprovalRequest, Approver, ApproverFailure};
use aither_sandbox::SandboxMode;

use crate::live_response::LiveResponseRegistry;

/// The [`aither_core::llm::ApprovalKind`] of a script that runs outside the
/// sandbox.
pub const UNSANDBOXED_KIND: &str = "unsandboxed";

/// The kind of a script that runs on an SSH server; the request's reason is
/// the server's target.
pub const SSH_KIND: &str = "ssh";

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

/// Error returned when parsing an unknown [`PermissionRequestMode`] value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission request mode: {value}")]
pub struct PermissionRequestModeParseError {
    value: String,
}

/// Scope of an in-flight permission escalation request.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRequestMode {
    /// Standard permission flow.
    Default,
    /// Unsafe (unsandboxed) permission request.
    Unsafe,
    /// Permission scoped to an SSH server.
    Ssh,
    /// Publishing an issue, pull request or comment.
    Publish,
}

impl PermissionRequestMode {
    /// Canonical serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Unsafe => "unsafe",
            Self::Ssh => "ssh",
            Self::Publish => "publish",
        }
    }
}

impl std::fmt::Display for PermissionRequestMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for PermissionRequestMode {
    type Err = PermissionRequestModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "default" => Ok(Self::Default),
            "unsafe" => Ok(Self::Unsafe),
            "ssh" => Ok(Self::Ssh),
            "publish" => Ok(Self::Publish),
            other => Err(PermissionRequestModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

impl From<SandboxMode> for PermissionRequestMode {
    fn from(value: SandboxMode) -> Self {
        match value {
            SandboxMode::Sandboxed => Self::Default,
            SandboxMode::Unsandboxed => Self::Unsafe,
        }
    }
}

/// Historical bucket used to record approved permission modes in
/// session history so future requests can be auto-approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionHistoryMode {
    /// Sandboxed-mode approval history.
    Sandboxed,
    /// Unsafe-mode approval history.
    Unsafe,
    /// SSH-scoped approval history.
    Ssh,
    /// Approval history of published issues, pull requests and comments.
    Publish,
}

impl PermissionHistoryMode {
    /// Canonical serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandboxed => "sandboxed",
            Self::Unsafe => "unsafe",
            Self::Ssh => "ssh",
            Self::Publish => "publish",
        }
    }
}

/// A persisted permission-history mode string did not match a known variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission history mode: {value}")]
pub struct PermissionHistoryModeParseError {
    value: String,
}

impl std::fmt::Display for PermissionHistoryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for PermissionHistoryMode {
    type Err = PermissionHistoryModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sandboxed" => Ok(Self::Sandboxed),
            "unsafe" => Ok(Self::Unsafe),
            "ssh" => Ok(Self::Ssh),
            "publish" => Ok(Self::Publish),
            other => Err(PermissionHistoryModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

impl From<SandboxMode> for PermissionHistoryMode {
    fn from(value: SandboxMode) -> Self {
        match value {
            SandboxMode::Sandboxed => Self::Sandboxed,
            SandboxMode::Unsandboxed => Self::Unsafe,
        }
    }
}

/// Serialized payload shown to the user/UI when an escalation is pending.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    /// Permission mode being requested.
    pub mode: PermissionRequestMode,
    /// Script or command the request covers.
    pub script: String,
    /// Session the request belongs to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Agent that raised the request: the session's own agent or one of its
    /// subagents, each with an instance-unique `word-word-word` id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// How May reads the script; absent for a request that covers no script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<crate::command_reading::CommandReading>,
}

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
    pub(crate) runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    approval_mode: ApprovalMode,
    timeout: std::time::Duration,
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
        }
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
            on_timeout,
        )
        .await
    }

    /// Persist the request, surface it to the UI and wait for the answer.
    async fn wait(
        &self,
        mode: PermissionRequestMode,
        history: PermissionHistoryMode,
        what: &str,
        script: String,
        on_timeout: OnTimeout,
    ) -> Result<bool, PermissionError> {
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
        };
        let request_id = crate::id::new();
        let rx = self.live_responses.open(&request_id);

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

        if let Ok(decision) = tokio::time::timeout(self.timeout, rx.recv()).await {
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
        match self
            .request_and_wait(SandboxMode::Unsandboxed, request.subject().to_string())
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
            .request_and_wait(SandboxMode::Unsandboxed, "cargo build".to_string())
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
                .request_and_wait(SandboxMode::Unsandboxed, "cargo build".to_string())
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
