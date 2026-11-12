//! Container runtime detection and management for Docker-compatible engines.
//!
//! Provides container-based sandbox isolation as an alternative to the native
//! heel sandbox.

pub mod browser;
pub mod image;
pub mod image_ref;
pub mod lifecycle;
pub mod profile;
pub mod stage;

use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
use crate::tools::HostProgram;
use aither_sandbox::{ContainerRuntimeKind, RuntimePreference};
use bollard::Docker;
#[cfg(target_os = "macos")]
use tokio::process::Command;

/// How long a socket probe waits for the runtime to answer its ping.
const DOCKER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The per-request timeout of the client kept for container operations:
/// bollard's own default.
const DOCKER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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
    Tools(#[from] crate::tools::ToolRuntimeError),
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

/// The hostname that resolves to the host machine from inside a container of
/// `kind`.
const fn host_gateway_name(kind: ContainerRuntimeKind) -> &'static str {
    match kind {
        ContainerRuntimeKind::Podman => "host.containers.internal",
        _ => "host.docker.internal",
    }
}

/// The Docker Desktop route to the host's IPC bridge, used on macOS and
/// Windows hosts.
///
/// Docker Desktop runs its engine in a VM and forwards the host gateway name
/// to the host's loopback interface, so the bridge binds loopback and the
/// container dials the gateway name. `OrbStack` and Podman machine forward
/// their gateway names the same way.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn docker_desktop_ipc_route(kind: ContainerRuntimeKind) -> aither_sandbox::HostRoute {
    aither_sandbox::HostRoute::new(
        host_gateway_name(kind),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    )
    .expect("loopback is a usable host route")
}

/// The gateway address a container uses to reach its host, selected from the
/// container's own inspection: exactly one distinct non-empty reported
/// gateway, and that address binds the IPC bridge.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn select_ipc_gateway(
    inspect: &bollard::models::ContainerInspectResponse,
    container_id: &str,
    host_name: &str,
) -> Result<aither_sandbox::HostRoute, ContainerError> {
    let mut gateways: Vec<String> = inspect
        .network_settings
        .as_ref()
        .and_then(|settings| settings.networks.as_ref())
        .map(|networks| {
            networks
                .values()
                .filter_map(|endpoint| endpoint.gateway.as_deref())
                .filter(|gateway| !gateway.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    gateways.sort_unstable();
    gateways.dedup();
    match gateways.as_slice() {
        [] => Err(ContainerError::IpcRouteNoGateway(container_id.to_string())),
        [gateway] => {
            let address = gateway.parse::<std::net::IpAddr>().map_err(|error| {
                ContainerError::IpcRouteUnusableGateway(
                    container_id.to_string(),
                    gateway.clone(),
                    error.to_string(),
                )
            })?;
            aither_sandbox::HostRoute::new(host_name, address).map_err(|error| {
                ContainerError::IpcRouteUnusableGateway(
                    container_id.to_string(),
                    gateway.clone(),
                    error.to_string(),
                )
            })
        }
        _ => Err(ContainerError::IpcRouteConflictingGateways(
            container_id.to_string(),
            gateways,
        )),
    }
}

/// A detected and connected container runtime.
pub struct ContainerRuntime {
    /// Detected runtime kind.
    pub kind: ContainerRuntimeKind,
    /// Bollard client connected to the runtime.
    pub client: Docker,
}

/// Options controlling container-runtime detection.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeDetectOptions {
    /// Whether detection may download a managed Podman when no runtime is
    /// found.
    pub allow_managed_podman_download: bool,
}

impl std::fmt::Debug for ContainerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerRuntime")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl ContainerRuntime {
    /// The hostname that resolves to the host machine from inside a container.
    #[must_use]
    pub const fn host_gateway(&self) -> &'static str {
        host_gateway_name(self.kind)
    }

    /// The route a container uses to reach the host's IPC bridge: the
    /// [`host_gateway`](Self::host_gateway) name it dials plus the host
    /// address the bridge binds.
    ///
    /// The bridge executes host IPC commands for whoever connects, so the
    /// bind is never a wildcard: on macOS and Windows hosts Docker Desktop,
    /// `OrbStack` and Podman machine forward the gateway name to host loopback
    /// (see [`docker_desktop_ipc_route`]); on native Linux the bind is the
    /// container network's gateway as the host owns it.
    ///
    /// # Errors
    /// Fails when the container reports no gateway or conflicting gateways,
    /// when the gateway cannot bind the bridge, or when the platform cannot
    /// route host IPC at all.
    pub async fn ipc_route(
        &self,
        container_id: &str,
    ) -> Result<aither_sandbox::HostRoute, ContainerError> {
        self.resolve_ipc_route(container_id).await
    }

    /// The Docker Desktop route: the container's own network is irrelevant
    /// because the engine VM forwards the gateway name to host loopback.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn resolve_ipc_route(
        &self,
        _container_id: &str,
    ) -> std::future::Ready<Result<aither_sandbox::HostRoute, ContainerError>> {
        std::future::ready(Ok(docker_desktop_ipc_route(self.kind)))
    }

    /// The native Linux route.
    #[cfg(target_os = "linux")]
    async fn resolve_ipc_route(
        &self,
        container_id: &str,
    ) -> Result<aither_sandbox::HostRoute, ContainerError> {
        // Native Docker and Podman resolve the host gateway name to the
        // container network's gateway, so the bridge binds that exact
        // address — read from the container's own inspection, never a
        // hardcoded subnet.
        let inspect = self
            .client
            .inspect_container(
                container_id,
                None::<bollard::query_parameters::InspectContainerOptions>,
            )
            .await
            .map_err(ContainerError::Docker)?;
        select_ipc_gateway(&inspect, container_id, self.host_gateway())
    }

    /// Hosts that are neither Docker Desktop hosts nor native Linux.
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    fn resolve_ipc_route(
        &self,
        _container_id: &str,
    ) -> std::future::Ready<Result<aither_sandbox::HostRoute, ContainerError>> {
        std::future::ready(Err(ContainerError::IpcRouteUnsupportedPlatform))
    }

    /// Detect and connect to the best available container runtime.
    ///
    /// Default priority: `OrbStack` > Podman > Docker.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError`] when no runtime can be found or reached,
    /// including failures of a managed Podman bootstrap when one is attempted.
    pub async fn detect() -> Result<Self, ContainerError> {
        Self::detect_with_preference_and_options(
            RuntimePreference::default(),
            RuntimeDetectOptions::default(),
        )
        .await
    }

    /// Detect and connect to a container runtime using a specific selection order.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError`] when none of the runtimes in `preference`'s
    /// order can be found or reached.
    pub async fn detect_with_preference(
        preference: RuntimePreference,
    ) -> Result<Self, ContainerError> {
        Self::detect_with_preference_and_options(preference, RuntimeDetectOptions::default()).await
    }

    /// Detect and connect to a container runtime with explicit managed-runtime options.
    ///
    /// # Errors
    ///
    /// Returns [`ContainerError::NoRuntime`] when none of the candidates connect,
    /// or the errors of managed Podman bootstrap —
    /// [`ContainerError::Bootstrap`],
    /// [`ContainerError::ManagedPodmanDownloadApprovalRequired`],
    /// [`ContainerError::Io`] — when a managed download is attempted.
    pub async fn detect_with_preference_and_options(
        preference: RuntimePreference,
        options: RuntimeDetectOptions,
    ) -> Result<Self, ContainerError> {
        for kind in preference.order {
            for socket_path in socket_candidates(kind) {
                if let Some(rt) = Self::try_connect(&socket_path, kind).await {
                    tracing::info!(runtime = ?kind, socket = %socket_path.display(), "Detected container runtime");
                    return Ok(rt);
                }
            }
        }

        Self::managed_runtime(options)
            .await?
            .ok_or(ContainerError::NoRuntime)
    }

    /// Fall back to a May-managed Podman when no system runtime answered.
    #[cfg(target_os = "macos")]
    async fn managed_runtime(
        options: RuntimeDetectOptions,
    ) -> Result<Option<Self>, ContainerError> {
        Self::bootstrap_managed_podman(options).await
    }

    /// No managed runtime exists off macOS, so the download option has nothing
    /// to govern here.
    #[cfg(not(target_os = "macos"))]
    fn managed_runtime(
        _options: RuntimeDetectOptions,
    ) -> impl std::future::Future<Output = Result<Option<Self>, ContainerError>> {
        std::future::ready(Ok(None))
    }

    /// Try connecting to a Docker-compatible endpoint: a Unix socket on Unix
    /// hosts, a named pipe on Windows hosts.
    async fn try_connect(socket_path: &Path, kind: ContainerRuntimeKind) -> Option<Self> {
        if !socket_path.exists() {
            return None;
        }

        let Some(endpoint) = socket_path.to_str() else {
            tracing::warn!(socket = %socket_path.display(), "Runtime endpoint path is not UTF-8");
            return None;
        };
        // `connect_with_socket` dials a Unix socket on Unix and a named pipe
        // on Windows. bollard applies one timeout to every request, so only
        // the probe gets the short one: creating or starting a container on
        // a busy host can take longer.
        let client = Docker::connect_with_socket(
            endpoint,
            DOCKER_REQUEST_TIMEOUT.as_secs(),
            bollard::API_DEFAULT_VERSION,
        )
        .ok()?;
        let probe = client.clone().with_timeout(DOCKER_PROBE_TIMEOUT);

        match probe.ping().await {
            Ok(_) => Some(Self { kind, client }),
            Err(e) => {
                tracing::debug!(socket = %socket_path.display(), error = %e, "Runtime socket ping failed");
                None
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn bootstrap_managed_podman(
        options: RuntimeDetectOptions,
    ) -> Result<Option<Self>, ContainerError> {
        tracing::info!(
            "No system container runtime detected; preparing managed Podman virtual environment"
        );

        let may_dir = crate::config::MayConfig::may_dir()
            .map_err(|e| ContainerError::Bootstrap(format!("failed to resolve ~/.may: {e}")))?;
        let host_programs = may_dir.join("cache").join("host-programs");
        let distribution = crate::tools::embedded_distribution()?;
        let target = crate::tools::ToolTarget::native()?;

        // A prepared binary that still has its pinned digest needs no
        // download and no approval; anything else is fetched through the
        // tool distribution, which verifies it before it is run.
        let podman_bin = if let Some(prepared) = distribution
            .cached_host_program(HostProgram::Podman, &target, &host_programs)
            .await?
        {
            prepared
        } else {
            if !options.allow_managed_podman_download {
                return Err(ContainerError::ManagedPodmanDownloadApprovalRequired);
            }
            tracing::info!("Downloading managed Podman");
            distribution
                .prepare_host_program(HostProgram::Podman, &target, &host_programs)
                .await?
        };

        let socket_path = ensure_managed_podman_machine(podman_bin.path()).await?;
        Ok(Self::try_connect(&socket_path, ContainerRuntimeKind::Podman).await)
    }
}

fn socket_candidates(kind: ContainerRuntimeKind) -> Vec<PathBuf> {
    match kind {
        ContainerRuntimeKind::OrbStack => dirs::home_dir().map_or_else(
            || {
                tracing::warn!("failed to resolve home directory while probing OrbStack socket");
                Vec::new()
            },
            |home| vec![home.join(".orbstack/run/docker.sock")],
        ),
        ContainerRuntimeKind::Podman => {
            let mut candidates = Vec::new();
            if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
                candidates.push(PathBuf::from(xdg).join("podman/podman.sock"));
            }
            if let Some(home) = dirs::home_dir() {
                candidates.push(home.join(".local/share/containers/podman/machine/podman.sock"));
            }
            candidates
        }
        ContainerRuntimeKind::Docker => vec![PathBuf::from(DOCKER_ENGINE_ENDPOINT)],
    }
}

/// The Docker Engine's local endpoint: the Unix socket on Unix hosts.
#[cfg(unix)]
const DOCKER_ENGINE_ENDPOINT: &str = "/var/run/docker.sock";

/// The Docker Engine's local endpoint: the named pipe Docker Desktop serves
/// on Windows hosts.
#[cfg(windows)]
const DOCKER_ENGINE_ENDPOINT: &str = r"\\.\pipe\docker_engine";

#[cfg(target_os = "macos")]
async fn ensure_managed_podman_machine(podman_bin: &Path) -> Result<PathBuf, ContainerError> {
    let machine_name = "may";

    if command_status(podman_bin, ["machine", "inspect", machine_name])
        .await
        .is_err()
    {
        run_command_checked(podman_bin, ["machine", "init", machine_name]).await?;
    }

    let state = run_command_checked(
        podman_bin,
        ["machine", "inspect", machine_name, "--format", "{{.State}}"],
    )
    .await?;

    if state.trim() != "Running" {
        run_command_checked(podman_bin, ["machine", "start", machine_name]).await?;
    }

    let socket = run_command_checked(
        podman_bin,
        [
            "machine",
            "inspect",
            machine_name,
            "--format",
            "{{.ConnectionInfo.PodmanSocket.Path}}",
        ],
    )
    .await?;
    let socket = socket.trim();
    if socket.is_empty() {
        return Err(ContainerError::Bootstrap(
            "managed Podman machine returned an empty socket path".to_string(),
        ));
    }

    Ok(PathBuf::from(socket))
}

#[cfg(target_os = "macos")]
async fn command_status(program: &Path, args: [&str; 3]) -> Result<(), ContainerError> {
    let status = Command::new(program)
        .args(args)
        .status()
        .await
        .map_err(|e| {
            ContainerError::Bootstrap(format!(
                "failed to execute {} {}: {e}",
                program.display(),
                args.join(" ")
            ))
        })?;

    if status.success() {
        Ok(())
    } else {
        Err(ContainerError::Bootstrap(format!(
            "command failed: {} {} (status {status})",
            program.display(),
            args.join(" ")
        )))
    }
}

#[cfg(target_os = "macos")]
async fn run_command_checked<const N: usize>(
    program: &Path,
    args: [&str; N],
) -> Result<String, ContainerError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            ContainerError::Bootstrap(format!(
                "failed to execute {} {}: {e}",
                program.display(),
                args.join(" ")
            ))
        })?;

    if !output.status.success() {
        return Err(ContainerError::Bootstrap(format!(
            "command failed: {} {} (status {}) stderr: {}",
            program.display(),
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|e| {
        ContainerError::Bootstrap(format!(
            "command output is not valid UTF-8: {} {}: {e}",
            program.display(),
            args.join(" ")
        ))
    })?;

    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::{ContainerInspectResponse, EndpointSettings, NetworkSettings};
    use std::collections::HashMap;

    fn inspect_with_gateways(gateways: &[&str]) -> ContainerInspectResponse {
        let networks: HashMap<String, EndpointSettings> = gateways
            .iter()
            .enumerate()
            .map(|(index, gateway)| {
                (
                    format!("net{index}"),
                    EndpointSettings {
                        gateway: Some((*gateway).to_string()),
                        ..Default::default()
                    },
                )
            })
            .collect();
        ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn select_ipc_gateway_uses_the_inspected_gateway() {
        let bind = select_ipc_gateway(
            &inspect_with_gateways(&["172.30.5.1"]),
            "container",
            "host.docker.internal",
        )
        .expect("a reported gateway binds the bridge");
        assert_eq!(
            bind.address(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(172, 30, 5, 1))
        );
    }

    #[test]
    fn select_ipc_gateway_errors_without_a_gateway() {
        for inspect in [
            ContainerInspectResponse::default(),
            inspect_with_gateways(&[""]),
        ] {
            assert!(
                matches!(
                    select_ipc_gateway(&inspect, "container", "host.docker.internal"),
                    Err(ContainerError::IpcRouteNoGateway(_))
                ),
                "inspection without a non-empty gateway must fail"
            );
        }
    }

    #[test]
    fn select_ipc_gateway_errors_on_conflicting_gateways() {
        let error = select_ipc_gateway(
            &inspect_with_gateways(&["172.30.5.1", "172.17.0.1"]),
            "container",
            "host.docker.internal",
        )
        .expect_err("distinct gateways must conflict");
        assert!(matches!(
            error,
            ContainerError::IpcRouteConflictingGateways(_, _)
        ));
    }

    #[test]
    fn select_ipc_gateway_accepts_a_duplicate_gateway() {
        let bind = select_ipc_gateway(
            &inspect_with_gateways(&["172.30.5.1", "172.30.5.1"]),
            "container",
            "host.docker.internal",
        )
        .expect("a gateway repeated across networks is unambiguous");
        assert_eq!(
            bind.address(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(172, 30, 5, 1))
        );
    }

    #[test]
    fn select_ipc_gateway_rejects_unusable_gateways() {
        for gateway in ["not-an-ip", "0.0.0.0", "224.0.0.1"] {
            assert!(
                matches!(
                    select_ipc_gateway(
                        &inspect_with_gateways(&[gateway]),
                        "container",
                        "host.docker.internal"
                    ),
                    Err(ContainerError::IpcRouteUnusableGateway(_, _, _))
                ),
                "gateway {gateway} must be rejected"
            );
        }
    }

    #[test]
    fn docker_desktop_route_dials_host_docker_internal_over_loopback() {
        let route = docker_desktop_ipc_route(ContainerRuntimeKind::Docker);
        assert_eq!(route.name(), "host.docker.internal");
        assert_eq!(
            route.address(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
    }
}
