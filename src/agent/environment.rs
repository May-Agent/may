use std::path::Path;
use std::sync::Arc;

use aither_sandbox::{BollardContainerExec, ContainerExecHandle, ContainerLaunchSpec};

use crate::agent::AgentEnvironment;
use crate::container::ContainerRuntime;
use crate::container::lifecycle::ContainerManager;
use crate::db::entity::profiles;
use crate::profile::ProfileEnvironment;
use crate::session::SessionId;

#[derive(Debug, thiserror::Error)]
pub enum ResolveAgentEnvironmentError {
    #[error("remote profile model is missing")]
    MissingRemoteProfileModel,
    #[error("container profile model is missing")]
    MissingContainerProfileModel,
    #[error("failed to load config.toml for remote profile: {0}")]
    LoadRemoteConfig(crate::config::ConfigError),
    #[error("{0}")]
    ResolveRemoteServer(String),
    #[error("no container runtime available for container profile: {0}")]
    DetectContainerRuntime(crate::container::ContainerError),
    #[error("invalid container profile image tag: {0}")]
    ContainerImageTag(crate::container::ContainerError),
    #[error("invalid container profile configuration: {0}")]
    ContainerProfileConfig(crate::container::ContainerError),
    #[error("invalid container mount configuration: {0}")]
    ContainerMountConfig(crate::container::ContainerError),
    #[error("container image build failed: {0}")]
    EnsureContainerImage(crate::container::ContainerError),
    #[error("container start failed: {0}")]
    StartContainer(crate::container::ContainerError),
}

pub async fn resolve_agent_environment_for_session(
    profile_environment: Option<ProfileEnvironment>,
    profile_model: Option<&profiles::Model>,
    may_dir: &Path,
    sandbox_dir: &Path,
    session_id: &SessionId,
) -> Result<AgentEnvironment, ResolveAgentEnvironmentError> {
    match profile_environment {
        Some(ProfileEnvironment::Container { .. }) => {}
        Some(ProfileEnvironment::Remote { .. }) => {
            let profile =
                profile_model.ok_or(ResolveAgentEnvironmentError::MissingRemoteProfileModel)?;
            let config_path = may_dir.join("config.toml");
            let config = crate::config::MayConfig::load_from(&config_path)
                .await
                .map_err(ResolveAgentEnvironmentError::LoadRemoteConfig)?;
            let ssh_server =
                crate::remote_profile::resolve_remote_profile_server(profile, &config.ssh_servers)
                    .map_err(ResolveAgentEnvironmentError::ResolveRemoteServer)?;
            return Ok(AgentEnvironment::Remote { ssh_server });
        }
        _ => return Ok(AgentEnvironment::Local),
    }

    let profile =
        profile_model.ok_or(ResolveAgentEnvironmentError::MissingContainerProfileModel)?;

    let runtime = ContainerRuntime::detect()
        .await
        .map_err(ResolveAgentEnvironmentError::DetectContainerRuntime)?;
    let container_manager = ContainerManager::new(Arc::new(runtime), may_dir.to_path_buf());

    let default_image_tag = crate::container::profile::default_image_tag(may_dir, profile)
        .await
        .map_err(ResolveAgentEnvironmentError::ContainerImageTag)?;
    let image_spec =
        crate::container::profile::image_spec_for_profile(may_dir, profile, &default_image_tag)
            .map_err(ResolveAgentEnvironmentError::ContainerProfileConfig)?;
    let extra_mounts = crate::container::profile::mounts_for_profile(profile)
        .map_err(ResolveAgentEnvironmentError::ContainerMountConfig)?;

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

    let container_exec = ContainerExecHandle::new(Arc::new(BollardContainerExec::new(Arc::new(
        container_manager.runtime().client.clone(),
    ))));

    Ok(AgentEnvironment::Container {
        container_id,
        container_ipc_host: container_manager.runtime().host_gateway().to_string(),
        container_exec,
        browser_backend: None,
    })
}
