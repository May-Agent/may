//! The permission-request modes persisted with a session.

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

/// Error returned when parsing an unknown [`PermissionRequestMode`] value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission request mode: {value}")]
pub struct PermissionRequestModeParseError {
    value: String,
}

/// A persisted permission-history mode string did not match a known variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission history mode: {value}")]
pub struct PermissionHistoryModeParseError {
    value: String,
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
    /// The interpreter the script runs under, so the prompt highlights it in
    /// its language; absent for a request that covers no script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub interpreter: Option<aither_sandbox::Interpreter>,
}

impl From<aither_sandbox::SandboxMode> for PermissionRequestMode {
    fn from(value: aither_sandbox::SandboxMode) -> Self {
        match value {
            aither_sandbox::SandboxMode::Sandboxed => Self::Default,
            aither_sandbox::SandboxMode::Unsandboxed => Self::Unsafe,
        }
    }
}

impl From<aither_sandbox::SandboxMode> for PermissionHistoryMode {
    fn from(value: aither_sandbox::SandboxMode) -> Self {
        match value {
            aither_sandbox::SandboxMode::Sandboxed => Self::Sandboxed,
            aither_sandbox::SandboxMode::Unsandboxed => Self::Unsafe,
        }
    }
}
