//! Container runtime detection and management for Docker-compatible engines.
//!
//! Provides container-based sandbox isolation as an alternative to the native
//! leash sandbox.

pub mod image;
pub mod lifecycle;
pub mod playwright;
pub mod profile;

use std::path::{Path, PathBuf};

use aither_sandbox::{ContainerRuntimeKind, RuntimePreference};
use bollard::Docker;
use tokio::process::Command;
use zenwave::ResponseExt;

/// Error type for container operations.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    #[error("Docker API error: {0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("No container runtime found (tried OrbStack, Podman, Docker)")]
    NoRuntime,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Container not found: {0}")]
    NotFound(String),
    #[error("Image build failed: {0}")]
    ImageBuildFailed(String),
    #[error("Container runtime bootstrap failed: {0}")]
    Bootstrap(String),
    #[error("Managed Podman download requires explicit user approval before downloading binaries")]
    ManagedPodmanDownloadApprovalRequired,
}

/// A detected and connected container runtime.
pub struct ContainerRuntime {
    pub kind: ContainerRuntimeKind,
    pub client: Docker,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RuntimeDetectOptions {
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
        match self.kind {
            ContainerRuntimeKind::Podman => "host.containers.internal",
            _ => "host.docker.internal",
        }
    }

    /// Detect and connect to the best available container runtime.
    ///
    /// Default priority: `OrbStack` > Podman > Docker.
    pub async fn detect() -> Result<Self, ContainerError> {
        Self::detect_with_preference_and_options(
            RuntimePreference::default(),
            RuntimeDetectOptions::default(),
        )
        .await
    }

    /// Detect and connect to a container runtime using a specific selection order.
    pub async fn detect_with_preference(
        preference: RuntimePreference,
    ) -> Result<Self, ContainerError> {
        Self::detect_with_preference_and_options(preference, RuntimeDetectOptions::default()).await
    }

    /// Detect and connect to a container runtime with explicit managed-runtime options.
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

        #[cfg(target_os = "macos")]
        {
            if let Some(rt) = Self::bootstrap_managed_podman(options).await? {
                return Ok(rt);
            }
        }

        Err(ContainerError::NoRuntime)
    }

    /// Try connecting to a Docker-compatible socket.
    async fn try_connect(socket_path: &Path, kind: ContainerRuntimeKind) -> Option<Self> {
        if !socket_path.exists() {
            return None;
        }

        let url = format!("unix://{}", socket_path.display());
        let client = Docker::connect_with_socket(&url, 5, bollard::API_DEFAULT_VERSION).ok()?;

        match client.ping().await {
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
        let bin_dir = may_dir.join("bin");
        tokio::fs::create_dir_all(&bin_dir).await?;
        let podman_bin = bin_dir.join("podman");

        if !tokio::fs::try_exists(&podman_bin).await? {
            if !options.allow_managed_podman_download {
                return Err(ContainerError::ManagedPodmanDownloadApprovalRequired);
            }
            download_managed_podman_binary(&podman_bin, &may_dir).await?;
        }

        let socket_path = ensure_managed_podman_machine(&podman_bin).await?;
        Ok(Self::try_connect(&socket_path, ContainerRuntimeKind::Podman).await)
    }
}

fn socket_candidates(kind: ContainerRuntimeKind) -> Vec<PathBuf> {
    match kind {
        ContainerRuntimeKind::OrbStack => dirs::home_dir()
            .map(|home| vec![home.join(".orbstack/run/docker.sock")])
            .unwrap_or_else(|| {
                tracing::warn!("failed to resolve home directory while probing OrbStack socket");
                Vec::new()
            }),
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
        ContainerRuntimeKind::Docker => vec![PathBuf::from("/var/run/docker.sock")],
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, serde::Deserialize)]
struct GithubRelease {
    assets: Vec<GithubAsset>,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[cfg(target_os = "macos")]
fn podman_asset_name_candidates_for_current_arch() -> Result<&'static [&'static str], ContainerError>
{
    match std::env::consts::ARCH {
        // Newer Podman releases ship `.zip`; older releases used `.tar.gz`.
        "aarch64" => Ok(&[
            "podman-remote-release-darwin_arm64.zip",
            "podman-remote-release-darwin_arm64.tar.gz",
        ]),
        "x86_64" => Ok(&[
            "podman-remote-release-darwin_amd64.zip",
            "podman-remote-release-darwin_amd64.tar.gz",
        ]),
        other => Err(ContainerError::Bootstrap(format!(
            "unsupported macOS architecture for managed Podman bootstrap: {other}"
        ))),
    }
}

#[cfg(target_os = "macos")]
async fn download_managed_podman_binary(
    podman_bin: &Path,
    may_dir: &Path,
) -> Result<(), ContainerError> {
    let release_response =
        zenwave::get("https://api.github.com/repos/containers/podman/releases/latest")
            .await
            .map_err(|e| {
                ContainerError::Bootstrap(format!("failed to query Podman release metadata: {e}"))
            })?;
    let release: GithubRelease = release_response.into_json().await.map_err(|e| {
        ContainerError::Bootstrap(format!("failed to parse Podman release metadata: {e}"))
    })?;

    let asset_name_candidates = podman_asset_name_candidates_for_current_arch()?;
    let asset = asset_name_candidates
        .iter()
        .find_map(|candidate| {
            release
                .assets
                .iter()
                .find(|asset| asset.name == *candidate)
                .cloned()
        })
        .ok_or_else(|| {
            ContainerError::Bootstrap(format!(
                "Podman release asset not found: expected one of [{}]",
                asset_name_candidates.join(", ")
            ))
        })?;

    tracing::info!(asset = %asset.name, "Downloading managed Podman binary");
    let archive_response = zenwave::get(asset.browser_download_url.as_str())
        .await
        .map_err(|e| ContainerError::Bootstrap(format!("failed to download Podman asset: {e}")))?;
    let bytes = archive_response.into_bytes().await.map_err(|e| {
        ContainerError::Bootstrap(format!("failed to read Podman asset bytes: {e}"))
    })?;

    let archive_path = may_dir.join(&asset.name);
    tokio::fs::write(&archive_path, bytes.as_ref()).await?;
    extract_podman_binary(&archive_path, &asset.name, podman_bin).await?;
    tokio::fs::remove_file(&archive_path).await.map_err(|e| {
        ContainerError::Bootstrap(format!("failed to remove temporary Podman archive: {e}"))
    })?;
    Ok(())
}

#[cfg(target_os = "macos")]
async fn extract_podman_binary(
    archive_path: &Path,
    archive_name: &str,
    destination: &Path,
) -> Result<(), ContainerError> {
    if archive_name.ends_with(".zip") {
        return extract_podman_binary_from_zip(archive_path, destination).await;
    }
    if archive_name.ends_with(".tar.gz") {
        return extract_podman_binary_from_tar_gz(archive_path, destination).await;
    }
    Err(ContainerError::Bootstrap(format!(
        "unsupported Podman archive format for {archive_name}"
    )))
}

#[cfg(target_os = "macos")]
async fn extract_podman_binary_from_tar_gz(
    archive_path: &Path,
    destination: &Path,
) -> Result<(), ContainerError> {
    let archive_path = archive_path.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::fs::File;

        use flate2::read::GzDecoder;

        let file = File::open(&archive_path)?;
        let decoder = GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let mut extracted = false;

        for entry in archive.entries()? {
            let mut entry = entry?;
            let entry_path = entry.path()?.into_owned();
            let file_name = entry_path.file_name().and_then(|name| name.to_str());
            if file_name == Some("podman") {
                let parent = destination.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid podman destination path: {}", destination.display()),
                    )
                })?;
                std::fs::create_dir_all(parent)?;
                entry.unpack(&destination)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))?;
                }
                extracted = true;
                break;
            }
        }

        if !extracted {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "podman binary not found in archive {}",
                    archive_path.display()
                ),
            ));
        }

        Ok::<(), std::io::Error>(())
    })
    .await
    .map_err(|e| ContainerError::Bootstrap(format!("podman extraction task failed: {e}")))?
    .map_err(|e| ContainerError::Bootstrap(format!("failed to extract Podman binary: {e}")))?;

    Ok(())
}

#[cfg(target_os = "macos")]
async fn extract_podman_binary_from_zip(
    archive_path: &Path,
    destination: &Path,
) -> Result<(), ContainerError> {
    let archive_path = archive_path.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::fs::File;
        use std::io;

        let file = File::open(&archive_path)?;
        let mut archive = zip::ZipArchive::new(file).map_err(io::Error::other)?;
        let mut extracted = false;

        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(io::Error::other)?;
            let Some(file_name) = std::path::Path::new(entry.name())
                .file_name()
                .and_then(|name| name.to_str())
            else {
                continue;
            };
            if file_name != "podman" {
                continue;
            }

            let parent = destination.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid podman destination path: {}", destination.display()),
                )
            })?;
            std::fs::create_dir_all(parent)?;
            let mut out = File::create(&destination)?;
            std::io::copy(&mut entry, &mut out)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o755))?;
            }
            extracted = true;
            break;
        }

        if !extracted {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "podman binary not found in archive {}",
                    archive_path.display()
                ),
            ));
        }

        Ok::<(), io::Error>(())
    })
    .await
    .map_err(|e| ContainerError::Bootstrap(format!("podman extraction task failed: {e}")))?
    .map_err(|e| ContainerError::Bootstrap(format!("failed to extract Podman binary: {e}")))?;

    Ok(())
}

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
