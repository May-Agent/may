//! Container lifecycle management: create, start, stop, remove, auto-stop.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

use aither_sandbox::{ContainerLaunchSpec, MountAccess};
use arc_swap::ArcSwap;
use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use bollard::query_parameters::{
    CreateContainerOptionsBuilder, ListContainersOptionsBuilder, RemoveContainerOptionsBuilder,
    StopContainerOptionsBuilder,
};
use tokio::time::Instant;

use super::{ContainerError, ContainerRuntime};
use crate::session::MayHomePaths;

const LABEL_MANAGED: &str = "may.managed";
const LABEL_SPEC_FINGERPRINT: &str = "may.spec-fingerprint";
const SPEC_FINGERPRINT_REVISION: &str = "2026-03-17-v8";

/// Default inactivity timeout before auto-stopping a container (in minutes).
const DEFAULT_AUTO_STOP_MINUTES: u64 = 30;

#[derive(Debug, Clone)]
struct SharedHostPaths {
    memory_file: PathBuf,
    tools_mount: aither_sandbox::MountSpec,
}

/// Manages container lifecycle for profile-based isolation.
pub struct ContainerManager {
    runtime: Arc<ContainerRuntime>,
    may_dir: PathBuf,
    /// Last activity timestamp per container ID.
    activity: Arc<ArcSwap<HashMap<String, Instant>>>,
}

impl ContainerManager {
    #[must_use]
    pub fn new(runtime: Arc<ContainerRuntime>, may_dir: PathBuf) -> Self {
        Self {
            runtime,
            may_dir,
            activity: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Get the underlying runtime.
    #[must_use]
    pub fn runtime(&self) -> &ContainerRuntime {
        &self.runtime
    }

    /// Ensure a container is running for the given profile.
    ///
    /// - If the container exists and is running, returns its ID.
    /// - If the container exists but is stopped, starts it.
    /// - If the container doesn't exist, creates and starts it.
    pub async fn ensure_running(
        &self,
        spec: &ContainerLaunchSpec,
    ) -> Result<String, ContainerError> {
        use bollard::models::ContainerSummaryStateEnum;

        let host_gateway = self.runtime.host_gateway().to_string();
        let shared_paths = self.ensure_shared_paths(spec).await?;
        let spec_fingerprint = Self::compute_spec_fingerprint(spec, &host_gateway, &shared_paths)?;

        if let Some(container) = self.find_container_by_name(spec.name.as_str()).await? {
            let id = container
                .id
                .as_deref()
                .ok_or_else(|| ContainerError::NotFound(spec.name.clone()))?
                .to_string();
            let is_running = container.state == Some(ContainerSummaryStateEnum::RUNNING);
            let labels = container.labels.clone().unwrap_or_default();
            let is_managed = labels.get(LABEL_MANAGED).map(String::as_str) == Some("true");
            if !is_managed {
                return Err(ContainerError::Bootstrap(format!(
                    "container name collision with unmanaged container: {} ({id})",
                    spec.name
                )));
            }
            let same_image = container.image.as_deref() == Some(spec.image.as_str());
            let same_fingerprint = labels.get(LABEL_SPEC_FINGERPRINT).map(String::as_str)
                == Some(spec_fingerprint.as_str());

            if !same_image || !same_fingerprint {
                tracing::info!(
                    container_id = %id,
                    container_name = %spec.name,
                    same_image,
                    same_fingerprint,
                    "Container spec drift detected, recreating container"
                );
                self.remove(&id).await?;
                let new_id = self
                    .create_container(spec, &host_gateway, &shared_paths, &spec_fingerprint)
                    .await?;
                self.runtime.client.start_container(&new_id, None).await?;
                self.touch_activity(&new_id);
                tracing::info!(
                    container_id = %new_id,
                    name = %spec.name,
                    "Recreated container with latest launch spec"
                );
                return Ok(new_id);
            }

            if is_running {
                self.touch_activity(&id);
                return Ok(id);
            }

            // Container exists but isn't running — start it
            self.runtime.client.start_container(&id, None).await?;
            self.touch_activity(&id);
            tracing::info!(container_id = %id, "Started existing container");
            return Ok(id);
        }

        // Create new container
        let id = self
            .create_container(spec, &host_gateway, &shared_paths, &spec_fingerprint)
            .await?;

        // Start it
        self.runtime.client.start_container(&id, None).await?;
        self.touch_activity(&id);
        tracing::info!(container_id = %id, name = %spec.name, "Created and started container");

        Ok(id)
    }

    /// Resolve a managed container id by deterministic container name.
    pub async fn resolve_container_id_by_name(
        &self,
        name: &str,
    ) -> Result<Option<String>, ContainerError> {
        let maybe = self.find_container_by_name(name).await?;
        Ok(maybe.and_then(|container| container.id))
    }

    /// Resolve a managed container id by name, but only when it is running.
    pub async fn resolve_running_container_id_by_name(
        &self,
        name: &str,
    ) -> Result<Option<String>, ContainerError> {
        use bollard::models::ContainerSummaryStateEnum;

        let maybe = self.find_container_by_name(name).await?;
        Ok(maybe.and_then(|container| {
            if container.state == Some(ContainerSummaryStateEnum::RUNNING) {
                return container.id;
            }
            None
        }))
    }

    /// Create a container (does not start it).
    async fn create_container(
        &self,
        spec: &ContainerLaunchSpec,
        host_gateway: &str,
        shared_paths: &SharedHostPaths,
        spec_fingerprint: &str,
    ) -> Result<String, ContainerError> {
        let env = Self::effective_env(spec, host_gateway);
        let mounts = Self::effective_mounts(spec, shared_paths)?;

        let host_config = HostConfig {
            mounts: Some(mounts),
            // Add host.docker.internal / host.containers.internal
            extra_hosts: Some(vec![format!("{host_gateway}:host-gateway")]),
            ..Default::default()
        };

        let labels = [
            (LABEL_MANAGED.to_string(), "true".to_string()),
            (
                LABEL_SPEC_FINGERPRINT.to_string(),
                spec_fingerprint.to_string(),
            ),
        ]
        .into_iter()
        .collect();

        let config = ContainerCreateBody {
            image: Some(spec.image.clone()),
            hostname: Some(spec.name.clone()),
            working_dir: Some("/workspace".to_string()),
            env: Some(env),
            host_config: Some(host_config),
            // Keep container alive with a long-running process
            cmd: Some(vec!["sleep".to_string(), "infinity".to_string()]),
            labels: Some(labels),
            ..Default::default()
        };

        let opts = CreateContainerOptionsBuilder::default()
            .name(&spec.name)
            .build();

        let response = self
            .runtime
            .client
            .create_container(Some(opts), config)
            .await?;

        Ok(response.id)
    }

    async fn ensure_shared_paths(
        &self,
        spec: &ContainerLaunchSpec,
    ) -> Result<SharedHostPaths, ContainerError> {
        let (image, distribution) = futures_lite::future::try_zip(
            async {
                self.runtime
                    .client
                    .inspect_image(&spec.image)
                    .await
                    .map_err(ContainerError::Docker)
            },
            async {
                crate::config::MayConfig::load_from(&self.may_dir.join("config.toml"))
                    .await?
                    .tools
                    .load_distribution()
                    .await
                    .map_err(ContainerError::Tools)
            },
        )
        .await?;
        let target = aither_sandbox::ToolTarget::from_probe(
            image.os.as_deref().ok_or_else(|| {
                ContainerError::Bootstrap("container image has no operating system".into())
            })?,
            image.architecture.as_deref().ok_or_else(|| {
                ContainerError::Bootstrap("container image has no architecture".into())
            })?,
        )?;
        let bundle = distribution
            .load(&target, &self.may_dir.join("tools/artifacts"))
            .await?;
        let (tools_mount, host_tools) = bundle
            .prepare_container_mount(&self.may_dir.join("tools/container-runtimes"))
            .await?;
        let writable = std::iter::once(&spec.workspace).chain(
            spec.mounts
                .iter()
                .filter(|mount| mount.access == MountAccess::ReadWrite)
                .map(|mount| &mount.host_path),
        );
        host_tools.ensure_readonly(writable).await?;
        let home_paths = MayHomePaths::new(self.may_dir.clone());
        let memory_file = home_paths.memory_file();
        if tokio::fs::metadata(&memory_file).await.is_err() {
            tokio::fs::write(&memory_file, "").await?;
        }

        tokio::fs::canonicalize(home_paths.root())
            .await
            .map_err(|error| {
                ContainerError::Bootstrap(format!(
                    "failed to canonicalize may dir {}: {error}",
                    self.may_dir.display()
                ))
            })?;
        let memory_file = tokio::fs::canonicalize(home_paths.memory_file())
            .await
            .map_err(|error| {
                ContainerError::Bootstrap(format!(
                    "failed to canonicalize global memory file {}: {error}",
                    home_paths.memory_file().display()
                ))
            })?;
        Ok(SharedHostPaths {
            memory_file,
            tools_mount,
        })
    }

    fn effective_env(spec: &ContainerLaunchSpec, host_gateway: &str) -> Vec<String> {
        let mut env = vec![
            "TERM=xterm-256color".to_string(),
            format!("MAY_HOST_GATEWAY={host_gateway}"),
        ];
        env.extend(spec.env.iter().cloned());
        env
    }

    fn effective_mounts(
        spec: &ContainerLaunchSpec,
        shared_paths: &SharedHostPaths,
    ) -> Result<Vec<Mount>, ContainerError> {
        let mut mounts = vec![
            // Session sandbox -> /workspace
            Mount {
                target: Some("/workspace".to_string()),
                source: Some(spec.workspace.display().to_string()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            },
            // Global memory file is shared across sessions.
            Mount {
                target: Some("/root/.may/MEMORY.md".to_string()),
                source: Some(shared_paths.memory_file.display().to_string()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            },
            // Session-scoped skill/subagent roots mapped into container ~/.may.
            Mount {
                target: Some("/root/.may/skills".to_string()),
                source: Some(spec.workspace.join("skills").display().to_string()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            },
            Mount {
                target: Some("/root/.may/subagents".to_string()),
                source: Some(spec.workspace.join("subagents").display().to_string()),
                typ: Some(MountType::BIND),
                read_only: Some(false),
                ..Default::default()
            },
        ];

        for mount in &spec.mounts {
            mounts.push(Mount {
                target: Some(mount.container_path.display().to_string()),
                source: Some(mount.host_path.display().to_string()),
                typ: Some(MountType::BIND),
                read_only: Some(mount.access == MountAccess::ReadOnly),
                ..Default::default()
            });
        }

        mounts.push(Mount {
            target: Some(
                shared_paths
                    .tools_mount
                    .container_path
                    .display()
                    .to_string(),
            ),
            source: Some(shared_paths.tools_mount.host_path.display().to_string()),
            typ: Some(MountType::BIND),
            read_only: Some(true),
            ..Default::default()
        });
        Ok(mounts)
    }

    fn compute_spec_fingerprint(
        spec: &ContainerLaunchSpec,
        host_gateway: &str,
        shared_paths: &SharedHostPaths,
    ) -> Result<String, ContainerError> {
        let mut env = Self::effective_env(spec, host_gateway);
        env.sort_unstable();

        let mut mounts: Vec<String> = Self::effective_mounts(spec, shared_paths)?
            .iter()
            .map(|mount| {
                format!(
                    "{}|{}|{}",
                    mount.source.as_deref().unwrap_or_default(),
                    mount.target.as_deref().unwrap_or_default(),
                    mount.read_only.unwrap_or(false)
                )
            })
            .collect();
        mounts.sort_unstable();

        let payload = serde_json::to_string(&serde_json::json!({
            "rev": SPEC_FINGERPRINT_REVISION,
            "name": spec.name,
            "image": spec.image,
            "workspace": spec.workspace.display().to_string(),
            "host_gateway": host_gateway,
            "env": env,
            "mounts": mounts,
        }))
        .map_err(|error| {
            ContainerError::Bootstrap(format!(
                "failed to serialize container spec fingerprint payload: {error}"
            ))
        })?;

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        payload.hash(&mut hasher);
        Ok(format!("{:016x}", hasher.finish()))
    }

    async fn find_container_by_name(
        &self,
        name: &str,
    ) -> Result<Option<bollard::models::ContainerSummary>, ContainerError> {
        let filters: HashMap<String, Vec<String>> =
            [("name".to_string(), vec![format!("^/{name}$")])].into();
        let opts = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();
        let existing = self.runtime.client.list_containers(Some(opts)).await?;
        Ok(existing.into_iter().next())
    }

    /// Stop a container gracefully.
    pub async fn stop(&self, container_id: &str) -> Result<(), ContainerError> {
        let opts = StopContainerOptionsBuilder::default().t(10).build();
        self.runtime
            .client
            .stop_container(container_id, Some(opts))
            .await?;
        self.update_activity(|activity| {
            activity.remove(container_id);
        });
        tracing::info!(container_id = %container_id, "Stopped container");
        Ok(())
    }

    /// Remove a container (force).
    pub async fn remove(&self, container_id: &str) -> Result<(), ContainerError> {
        let opts = RemoveContainerOptionsBuilder::default().force(true).build();
        self.runtime
            .client
            .remove_container(container_id, Some(opts))
            .await?;
        self.update_activity(|activity| {
            activity.remove(container_id);
        });
        tracing::info!(container_id = %container_id, "Removed container");
        Ok(())
    }

    /// Record activity for a container (resets auto-stop timer).
    pub fn touch_activity(&self, container_id: &str) {
        self.update_activity(|activity| {
            activity.insert(container_id.to_string(), Instant::now());
        });
    }

    /// Spawn a background task that auto-stops idle containers.
    #[must_use]
    pub fn spawn_auto_stop_task(&self) -> tokio::task::JoinHandle<()> {
        self.spawn_auto_stop_task_with_timeout(DEFAULT_AUTO_STOP_MINUTES)
    }

    /// Spawn auto-stop task with a custom timeout.
    #[must_use]
    pub fn spawn_auto_stop_task_with_timeout(
        &self,
        timeout_minutes: u64,
    ) -> tokio::task::JoinHandle<()> {
        let runtime = self.runtime.clone();
        let activity = self.activity.clone();
        let timeout = std::time::Duration::from_secs(timeout_minutes * 60);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;

                let now = Instant::now();
                let snapshot = activity.load();
                let idle_containers: Vec<String> = snapshot
                    .iter()
                    .filter(|(_, last_seen)| now.duration_since(**last_seen) > timeout)
                    .map(|(container_id, _)| container_id.clone())
                    .collect();

                for container_id in idle_containers {
                    let opts = StopContainerOptionsBuilder::default().t(10).build();
                    match runtime
                        .client
                        .stop_container(&container_id, Some(opts))
                        .await
                    {
                        Ok(()) => {
                            let mut next_activity = activity.load().as_ref().clone();
                            next_activity.remove(&container_id);
                            activity.store(Arc::new(next_activity));
                            tracing::info!(
                                container_id = %container_id,
                                "Auto-stopped idle container"
                            );
                        }
                        Err(e) => {
                            // Container may already be stopped or removed
                            tracing::debug!(
                                container_id = %container_id,
                                error = %e,
                                "Failed to auto-stop container"
                            );
                            let mut next_activity = activity.load().as_ref().clone();
                            next_activity.remove(&container_id);
                            activity.store(Arc::new(next_activity));
                        }
                    }
                }
            }
        })
    }

    fn update_activity(&self, update: impl FnOnce(&mut HashMap<String, Instant>)) {
        let mut activity = self.activity.load().as_ref().clone();
        update(&mut activity);
        self.activity.store(Arc::new(activity));
    }

    /// Check if a container is currently running.
    pub async fn is_running(&self, container_id: &str) -> Result<bool, ContainerError> {
        let info = self
            .runtime
            .client
            .inspect_container(container_id, None)
            .await?;
        let running = info.state.and_then(|s| s.running).unwrap_or(false);
        Ok(running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_paths() -> SharedHostPaths {
        SharedHostPaths {
            memory_file: "/managed/MEMORY.md".into(),
            tools_mount: aither_sandbox::MountSpec::read_only(
                "/managed/tools/bundle",
                "/opt/aither/tools/bundle",
            ),
        }
    }

    #[test]
    fn tool_mount_is_readonly_and_preserves_workspace_mounts() {
        let spec = ContainerLaunchSpec::new("test", "test-image", "/workspace-host").with_mount(
            aither_sandbox::MountSpec::read_write("/project-host", "/project"),
        );
        let shared = shared_paths();
        let mounts = ContainerManager::effective_mounts(&spec, &shared).unwrap();
        let tools = mounts
            .iter()
            .find(|mount| mount.target.as_deref() == Some("/opt/aither/tools/bundle"))
            .unwrap();
        assert_eq!(tools.source.as_deref(), Some("/managed/tools/bundle"));
        assert_eq!(tools.typ, Some(MountType::BIND));
        assert_eq!(tools.read_only, Some(true));
        for target in ["/workspace", "/project"] {
            assert_eq!(
                mounts
                    .iter()
                    .find(|mount| mount.target.as_deref() == Some(target))
                    .unwrap()
                    .read_only,
                Some(false)
            );
        }
    }

    #[test]
    fn bundle_change_invalidates_container_fingerprint() {
        let spec = ContainerLaunchSpec::new("test", "test-image", "/workspace-host");
        let first = shared_paths();
        let mut second = first.clone();
        second.tools_mount = aither_sandbox::MountSpec::read_only(
            "/managed/tools/new-bundle",
            "/opt/aither/tools/new-bundle",
        );
        assert_ne!(
            ContainerManager::compute_spec_fingerprint(&spec, "host.docker.internal", &first)
                .unwrap(),
            ContainerManager::compute_spec_fingerprint(&spec, "host.docker.internal", &second)
                .unwrap()
        );
    }

    #[test]
    fn tool_and_configuration_errors_remain_distinct() {
        let preparation = ContainerError::from(aither_sandbox::ToolRuntimeError::InvalidProbe);
        assert!(matches!(preparation, ContainerError::Tools(_)));
        let configuration = ContainerError::from(crate::config::ConfigError::NoHomeDir);
        assert!(matches!(
            configuration,
            ContainerError::ToolsConfiguration(_)
        ));
    }
}
