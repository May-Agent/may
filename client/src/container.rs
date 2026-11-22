//! The error type container operations fail with.

/// Error type for container operations.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// Docker API error.
    #[error("Docker API error: {0}")]
    Docker(#[from] bollard::errors::Error),
    /// No container runtime found (tried `OrbStack`, Podman, Docker).
    #[error("No container runtime found (tried OrbStack, Podman, Docker)")]
    NoRuntime,
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[doc = "Managed tools failed integrity or read-only preparation."]
    #[error("baseline tools preparation failed: {0}")]
    Tools(#[from] may_tool_dist::ToolRuntimeError),
    /// Container not found.
    #[error("Container not found: {0}")]
    NotFound(String),
    /// Image pull failed.
    #[error("Image pull failed: {0}")]
    ImagePullFailed(String),
    /// Container runtime bootstrap failed.
    #[error("Container runtime bootstrap failed: {0}")]
    Bootstrap(String),
    /// Managed Podman download requires explicit user approval before downloading binaries.
    #[error("Managed Podman download requires explicit user approval before downloading binaries")]
    ManagedPodmanDownloadApprovalRequired,
    /// The container exposes no gateway the host bridge can bind.
    #[error("container {0} exposes no gateway for the host IPC bridge")]
    IpcRouteNoGateway(String),
    /// The container's networks report different gateway addresses.
    #[error("container {0} reports conflicting gateways for the host IPC bridge: {1:?}")]
    IpcRouteConflictingGateways(String, Vec<String>),
    /// The container's gateway cannot bind the host bridge.
    #[error("container {0} gateway {1} is not a usable bridge bind: {2}")]
    IpcRouteUnusableGateway(String, String, String),
    /// The host platform has no container IPC bridge route: neither a
    /// Docker Desktop-family host (macOS, Windows) nor native Linux.
    #[error("container IPC bridge routes are unsupported on this platform")]
    IpcRouteUnsupportedPlatform,
}
