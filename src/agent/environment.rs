use std::path::Path;
use std::sync::Arc;

use aither_sandbox::{AnyContainerExec, BollardContainerExec, ContainerLaunchSpec};

use crate::agent::AgentEnvironment;
use crate::container::ContainerRuntime;
use crate::container::lifecycle::ContainerManager;
use crate::db::entity::profiles;
use crate::profile::ContainerProfilePreferences;
use crate::profile::ProfileEnvironment;
use crate::session::SessionId;

/// Error type for resolving the environment an agent session runs in.
#[derive(Debug, thiserror::Error)]
pub enum ResolveAgentEnvironmentError {
    /// Container profile model is missing.
    #[error("container profile model is missing")]
    MissingContainerProfileModel,
    /// No container runtime available for container profile.
    #[error("no container runtime available for container profile: {0}")]
    DetectContainerRuntime(crate::container::ContainerError),
    /// Invalid container profile image tag.
    #[error("invalid container profile image tag: {0}")]
    ContainerImageTag(crate::container::ContainerError),
    /// Invalid container profile configuration.
    #[error("invalid container profile configuration: {0}")]
    ContainerProfileConfig(crate::container::ContainerError),
    /// Invalid container mount configuration.
    #[error("invalid container mount configuration: {0}")]
    ContainerMountConfig(crate::container::ContainerError),
    /// Container image build failed.
    #[error("container image build failed: {0}")]
    EnsureContainerImage(crate::container::ContainerError),
    /// Container start failed.
    #[error("container start failed: {0}")]
    StartContainer(crate::container::ContainerError),
    /// The container's IPC bridge route could not be resolved.
    #[error("container IPC route resolution failed: {0}")]
    ResolveContainerIpcRoute(crate::container::ContainerError),
}

/// Resolve the container/local environment an agent session should run in,
/// deriving image tag, mounts, and tool environments from the profile.
///
/// # Errors
///
/// Returns [`ResolveAgentEnvironmentError`] when the profile requires a
/// container environment but its model row is missing.
pub async fn resolve_agent_environment_for_session(
    profile_environment: Option<ProfileEnvironment>,
    profile_model: Option<&profiles::Model>,
    container_preferences: Option<ContainerProfilePreferences>,
    may_dir: &Path,
    sandbox_dir: &Path,
    session_id: &SessionId,
    tool_distribution: &Arc<aither_sandbox::ToolDistribution>,
) -> Result<AgentEnvironment, ResolveAgentEnvironmentError> {
    match profile_environment {
        Some(ProfileEnvironment::Container { .. }) => {}
        _ => return Ok(AgentEnvironment::Local),
    }

    let profile =
        profile_model.ok_or(ResolveAgentEnvironmentError::MissingContainerProfileModel)?;

    let runtime = ContainerRuntime::detect()
        .await
        .map_err(ResolveAgentEnvironmentError::DetectContainerRuntime)?;
    let container_manager = ContainerManager::new(
        Arc::new(runtime),
        may_dir.to_path_buf(),
        tool_distribution.clone(),
    );

    let default_image_tag = Box::pin(crate::container::profile::default_image_tag(
        may_dir,
        profile,
        container_preferences.clone(),
    ))
    .await
    .map_err(ResolveAgentEnvironmentError::ContainerImageTag)?;
    let image_spec = crate::container::profile::image_spec_for_profile(
        may_dir,
        profile,
        &default_image_tag,
        container_preferences.clone(),
    );
    let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences);

    crate::container::image::ensure_image(container_manager.runtime(), &image_spec)
        .await
        .map_err(ResolveAgentEnvironmentError::EnsureContainerImage)?;

    let container_name =
        crate::container::profile::container_name_for_profile_session(profile, session_id);
    let mut launch_spec = ContainerLaunchSpec::new(
        container_name,
        image_spec.tag.clone(),
        sandbox_dir.to_path_buf(),
    );
    for mount in extra_mounts {
        launch_spec = launch_spec.with_mount(mount);
    }

    let container_id = container_manager
        .ensure_running(&launch_spec)
        .await
        .map_err(ResolveAgentEnvironmentError::StartContainer)?;

    let container_exec = AnyContainerExec::new(Arc::new(BollardContainerExec::new(Arc::new(
        container_manager.runtime().client.clone(),
    ))));

    let container_ipc_route = container_manager
        .runtime()
        .ipc_route(&container_id)
        .await
        .map_err(ResolveAgentEnvironmentError::ResolveContainerIpcRoute)?;

    Ok(AgentEnvironment::Container {
        container_id,
        container_ipc_route,
        container_exec,
        browser_cdp_endpoint: None,
    })
}
