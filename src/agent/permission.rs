//! Permission approval for terminal tool escalations.
//!
//! `PermissionApprover` is the [`aither_sandbox::PermissionHandler`]
//! implementation used by `MayAgent`. It binds to a session, routes
//! escalation requests through the runtime service so they are
//! persisted and visible in the UI, and blocks the terminal tool
//! until the user responds (or the request is interrupted).

use std::future::Future;
use std::sync::Arc;

use aither_sandbox::{
    TerminalMode,
    permission::{PermissionError, PermissionHandler},
};

use crate::live_response::LiveResponseRegistry;

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
}

impl PermissionRequestMode {
    /// Canonical serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Unsafe => "unsafe",
            Self::Ssh => "ssh",
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
            other => Err(PermissionRequestModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

impl From<TerminalMode> for PermissionRequestMode {
    fn from(value: TerminalMode) -> Self {
        match value {
            TerminalMode::Sandboxed => Self::Default,
            TerminalMode::Unsafe => Self::Unsafe,
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
}

impl PermissionHistoryMode {
    /// Canonical serialized name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sandboxed => "sandboxed",
            Self::Unsafe => "unsafe",
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
            other => Err(PermissionHistoryModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

impl From<TerminalMode> for PermissionHistoryMode {
    fn from(value: TerminalMode) -> Self {
        match value {
            TerminalMode::Sandboxed => Self::Sandboxed,
            TerminalMode::Unsafe => Self::Unsafe,
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
    pub(crate) runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    approval_mode: ApprovalMode,
}

impl PermissionApprover {
    /// Create an approver with no session binding.
    #[must_use]
    pub fn new() -> Self {
        Self {
            live_responses: Arc::new(LiveResponseRegistry::new()),
            session_id: None,
            runtime_service: None,
            approval_mode: ApprovalMode::Interactive,
        }
    }

    /// Return a copy bound to `session_id`.
    #[must_use]
    pub fn for_session(&self, session_id: impl Into<String>) -> Self {
        Self {
            live_responses: self.live_responses.clone(),
            session_id: Some(session_id.into()),
            runtime_service: self.runtime_service.clone(),
            approval_mode: self.approval_mode,
        }
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

    fn unavailable_error(mode: TerminalMode, context: &'static str) -> PermissionError {
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
            },
        }
    }

    async fn append_request_history(
        db: &crate::db::ChatDb,
        session_id: &str,
        request_id: &str,
        mode: TerminalMode,
        script: &str,
    ) -> Result<(), PermissionError> {
        db.append_permission_request_message(
            session_id,
            request_id,
            PermissionHistoryMode::from(mode),
            script,
            None,
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
        mode: TerminalMode,
        script: String,
    ) -> Result<bool, PermissionError> {
        if let ApprovalMode::Unavailable { context } = self.approval_mode {
            return Err(Self::unavailable_error(mode, context));
        }

        let Some(session_id) = self.session_id.clone() else {
            return Err(PermissionError::Internal(anyhow::anyhow!(
                "permission approval requires a bound session id"
            )));
        };

        let payload = PermissionRequest {
            mode: mode.into(),
            script,
            session_id: Some(session_id.clone()),
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let rx = self.live_responses.open(request_id.clone());

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
            Self::append_request_history(&db, &session_id, &request_id, mode, &payload.script)
                .await?;
        }

        rx.recv().await.map_err(|_| PermissionError::Interrupted)
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

impl PermissionHandler for PermissionApprover {
    async fn check(&self, mode: TerminalMode, script: &str) -> Result<bool, PermissionError> {
        match mode {
            TerminalMode::Sandboxed => Ok(true),
            TerminalMode::Unsafe => {
                let approved = self.request_and_wait(mode, script.to_string()).await?;
                if approved {
                    Ok(true)
                } else {
                    Err(PermissionError::Denied(format!(
                        "{} mode denied for script",
                        mode.description()
                    )))
                }
            }
        }
    }

    fn check_domain(&self, _domain: &str, _port: u16) -> impl Future<Output = bool> + Send {
        std::future::ready(true)
    }

    fn will_wait_for_approval(
        &self,
        mode: TerminalMode,
        _script: &str,
    ) -> impl Future<Output = bool> + Send {
        std::future::ready(
            !matches!(self.approval_mode, ApprovalMode::Unavailable { .. })
                && matches!(mode, TerminalMode::Unsafe),
        )
    }
}
