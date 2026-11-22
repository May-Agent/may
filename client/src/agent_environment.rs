//! The error an agent-environment resolution fails with.

/// Error type for resolving the environment an agent session runs in.
#[derive(Debug, thiserror::Error)]
pub enum ResolveAgentEnvironmentError {
    /// Container profile model is missing.
    #[error("container profile model is missing")]
    MissingContainerProfileModel,
    /// No container runtime available for container profile.
    #[error("no container runtime available for container profile: {0}")]
    DetectContainerRuntime(crate::container::ContainerError),
    /// Container image pull failed.
    #[error("container image pull failed: {0}")]
    EnsureContainerImage(crate::container::ContainerError),
    /// Container start failed.
    #[error("container start failed: {0}")]
    StartContainer(crate::container::ContainerError),
    /// The container's IPC bridge route could not be resolved.
    #[error("container IPC route resolution failed: {0}")]
    ResolveContainerIpcRoute(crate::container::ContainerError),
}
