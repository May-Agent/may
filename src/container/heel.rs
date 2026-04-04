//! Managed `heel` binary provisioning for container build contexts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::ContainerError;

const HEEL_RELEASE_API_URL: &str = "https://api.github.com/repos/lexoliu/heel/releases/latest";
const HEEL_RELEASE_STATE_FILE: &str = "release-state.json";
const HEEL_BINARY_FILE: &str = "heel";

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManagedHeelState {
    release_tag: String,
    asset_name: String,
}

pub async fn ensure_container_heel_binary(may_dir: &Path) -> Result<(), ContainerError> {
    let heel_dir = may_dir.join("container").join("heel");
    let bin_dir = heel_dir.join("bin");
    tokio::fs::create_dir_all(&bin_dir).await?;

    let binary_path = bin_dir.join(HEEL_BINARY_FILE);
    let state_path = heel_dir.join(HEEL_RELEASE_STATE_FILE);
    let asset_name = linux_asset_name_for_current_arch()?;
    let release = fetch_latest_release().await?;
    let asset = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset_name)
        .cloned()
        .ok_or_else(|| {
            ContainerError::Bootstrap(format!(
                "heel release asset not found: expected {asset_name}"
            ))
        })?;

    if binary_is_current(&binary_path, &state_path, &release.tag_name, &asset.name).await? {
        return Ok(());
    }

    tracing::info!(
        release_tag = %release.tag_name,
        asset = %asset.name,
        "Downloading managed heel binary"
    );
    let bytes = download_asset_bytes(&asset.browser_download_url).await?;
    install_binary(&binary_path, bytes.as_ref()).await?;
    write_state(
        &state_path,
        &ManagedHeelState {
            release_tag: release.tag_name,
            asset_name: asset.name,
        },
    )
    .await?;

    Ok(())
}

fn linux_asset_name_for_current_arch() -> Result<&'static str, ContainerError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("heel-x86_64-unknown-linux-gnu"),
        "aarch64" => Ok("heel-aarch64-unknown-linux-gnu"),
        other => Err(ContainerError::Bootstrap(format!(
            "unsupported host architecture for managed heel container binary: {other}"
        ))),
    }
}

async fn fetch_latest_release() -> Result<GithubRelease, ContainerError> {
    reqwest::Client::new()
        .get(HEEL_RELEASE_API_URL)
        .header(
            reqwest::header::USER_AGENT,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|error| {
            ContainerError::Bootstrap(format!("failed to query heel release metadata: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            ContainerError::Bootstrap(format!("heel release metadata request failed: {error}"))
        })?
        .json::<GithubRelease>()
        .await
        .map_err(|error| {
            ContainerError::Bootstrap(format!("failed to parse heel release metadata: {error}"))
        })
}

async fn download_asset_bytes(url: &str) -> Result<bytes::Bytes, ContainerError> {
    reqwest::Client::new()
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .map_err(|error| {
            ContainerError::Bootstrap(format!("failed to download managed heel binary: {error}"))
        })?
        .error_for_status()
        .map_err(|error| {
            ContainerError::Bootstrap(format!("managed heel download failed: {error}"))
        })?
        .bytes()
        .await
        .map_err(|error| {
            ContainerError::Bootstrap(format!(
                "failed to read managed heel binary payload: {error}"
            ))
        })
}

async fn binary_is_current(
    binary_path: &Path,
    state_path: &Path,
    release_tag: &str,
    asset_name: &str,
) -> Result<bool, ContainerError> {
    if !tokio::fs::try_exists(binary_path).await? {
        return Ok(false);
    }
    let Some(state) = read_state(state_path).await? else {
        return Ok(false);
    };
    Ok(state.release_tag == release_tag && state.asset_name == asset_name)
}

async fn read_state(path: &Path) -> Result<Option<ManagedHeelState>, ContainerError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            ContainerError::Bootstrap(format!(
                "failed to parse managed heel state {}: {error}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ContainerError::Io(error)),
    }
}

async fn write_state(path: &Path, state: &ManagedHeelState) -> Result<(), ContainerError> {
    let bytes = serde_json::to_vec(state).map_err(|error| {
        ContainerError::Bootstrap(format!(
            "failed to serialize managed heel state {}: {error}",
            path.display()
        ))
    })?;
    tokio::fs::write(path, bytes).await?;
    Ok(())
}

async fn install_binary(binary_path: &Path, bytes: &[u8]) -> Result<(), ContainerError> {
    let temporary_path = temporary_download_path(binary_path);
    tokio::fs::write(&temporary_path, bytes).await?;
    make_executable(&temporary_path).await?;
    if tokio::fs::try_exists(binary_path).await? {
        tokio::fs::remove_file(binary_path).await?;
    }
    tokio::fs::rename(&temporary_path, binary_path).await?;
    Ok(())
}

fn temporary_download_path(binary_path: &Path) -> PathBuf {
    let file_name = binary_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(HEEL_BINARY_FILE);
    binary_path.with_file_name(format!("{file_name}.download"))
}

#[cfg(unix)]
async fn make_executable(path: &Path) -> Result<(), ContainerError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = tokio::fs::metadata(path).await?.permissions();
    permissions.set_mode(0o755);
    tokio::fs::set_permissions(path, permissions).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn make_executable(_path: &Path) -> Result<(), ContainerError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_arch_maps_to_linux_release_asset_name() {
        let asset_name = linux_asset_name_for_current_arch().expect("supported test arch");
        assert!(asset_name.starts_with("heel-"));
        assert!(asset_name.ends_with("-unknown-linux-gnu"));
    }

    #[tokio::test]
    async fn managed_state_round_trips() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_path = temp.path().join(HEEL_RELEASE_STATE_FILE);
        let state = ManagedHeelState {
            release_tag: "v1.2.3".to_string(),
            asset_name: "heel-x86_64-unknown-linux-gnu".to_string(),
        };

        write_state(&state_path, &state).await.expect("write state");
        let parsed = read_state(&state_path)
            .await
            .expect("read state")
            .expect("state exists");
        assert_eq!(parsed, state);
    }
}
