pub use may_client::agent_environment::ResolveAgentEnvironmentError;

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

/// Resolve the container/local environment an agent session should run in,
/// deriving image tag, mounts, and tool environments from the profile.
///
/// # Errors
///
/// Returns [`ResolveAgentEnvironmentError`] when the profile requires a
/// container environment but its model row is missing, when no container
/// runtime can be detected, when the profile's image tag is invalid, when the
/// image fails to build, when the container fails to start, or when the
/// container's IPC route cannot be resolved.
pub async fn resolve_agent_environment_for_session(
    profile_environment: Option<ProfileEnvironment>,
    profile_model: Option<&profiles::Model>,
    container_preferences: Option<ContainerProfilePreferences>,
    may_dir: &Path,
    sandbox_dir: &Path,
    session_id: &SessionId,
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
    let container_manager = ContainerManager::new(Arc::new(runtime), may_dir.to_path_buf());

    let extra_mounts = crate::container::profile::mounts_for_profile(container_preferences);
    let image = crate::container::image::MAY_WORKSPACE_IMAGE;

    crate::container::image::ensure_image(container_manager.runtime(), image)
        .await
        .map_err(ResolveAgentEnvironmentError::EnsureContainerImage)?;

    let container_name =
        crate::container::profile::container_name_for_profile_session(profile, session_id);
    let mut launch_spec =
        ContainerLaunchSpec::new(container_name, image.to_string(), sandbox_dir.to_path_buf());
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
