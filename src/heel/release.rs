use std::path::{Path, PathBuf};

use async_fs;
use serde::{Deserialize, Serialize};
use zenwave::Client;

const HEEL_RELEASE_API_URL: &str = "https://api.github.com/repos/lexoliu/heel/releases/latest";
const HEEL_RELEASE_STATE_FILE: &str = "release-state.json";
const HEEL_BINARY_FILE: &str = "heel";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedHeelAsset {
    release_asset_name: &'static str,
    local_binary_name: &'static str,
}

impl ManagedHeelAsset {
    pub(crate) const fn release_asset_name(self) -> &'static str {
        self.release_asset_name
    }

    pub(crate) const fn local_binary_name(self) -> &'static str {
        self.local_binary_name
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedHeelError {
    #[error("unsupported heel host target: os={os} arch={arch}")]
    UnsupportedHostTarget {
        os: &'static str,
        arch: &'static str,
    },
    #[error("unsupported heel linux target architecture: {arch}")]
    UnsupportedLinuxArch { arch: &'static str },
    #[error("failed to create managed heel directory {path:?}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to build heel release metadata request")]
    MetadataRequest(#[source] zenwave::Error),
    #[error("failed to parse heel release metadata")]
    MetadataDecode(#[source] zenwave::Error),
    #[error("heel release asset not found: expected {asset_name} in release {release_tag}")]
    MissingReleaseAsset {
        release_tag: String,
        asset_name: &'static str,
    },
    #[error("failed to download heel asset {asset_name}")]
    AssetDownload {
        asset_name: String,
        #[source]
        source: zenwave::Error,
    },
    #[error("failed to read heel asset payload: {asset_name}")]
    AssetPayload {
        asset_name: String,
        #[source]
        source: zenwave::Error,
    },
    #[error("heel asset payload was empty: {asset_name}")]
    EmptyAssetPayload { asset_name: String },
    #[error("failed to parse managed heel state {path:?}")]
    StateParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize managed heel state {path:?}")]
    StateSerialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to {operation} {path:?}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

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
    #[serde(default)]
    last_checked_at: Option<String>,
}

/// How often to re-check the GitHub API for a new heel release (24 hours).
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Maps an OS/arch pair onto the managed heel release asset published for it.
fn host_heel_asset(
    os: &'static str,
    arch: &'static str,
) -> Result<ManagedHeelAsset, ManagedHeelError> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(ManagedHeelAsset {
            release_asset_name: "heel-aarch64-apple-darwin",
            local_binary_name: "heel",
        }),
        ("macos", "x86_64") => Ok(ManagedHeelAsset {
            release_asset_name: "heel-x86_64-apple-darwin",
            local_binary_name: "heel",
        }),
        ("linux", "aarch64") => Ok(ManagedHeelAsset {
            release_asset_name: "heel-aarch64-unknown-linux-gnu",
            local_binary_name: "heel",
        }),
        ("linux", "x86_64") => Ok(ManagedHeelAsset {
            release_asset_name: "heel-x86_64-unknown-linux-gnu",
            local_binary_name: "heel",
        }),
        ("windows", "x86_64") => Ok(ManagedHeelAsset {
            release_asset_name: "heel-x86_64-pc-windows-msvc.exe",
            local_binary_name: "heel.exe",
        }),
        ("windows", "aarch64") => Ok(ManagedHeelAsset {
            release_asset_name: "heel-aarch64-pc-windows-msvc.exe",
            local_binary_name: "heel.exe",
        }),
        (os, arch) => Err(ManagedHeelError::UnsupportedHostTarget { os, arch }),
    }
}

pub(crate) fn current_host_heel_asset() -> Result<ManagedHeelAsset, ManagedHeelError> {
    host_heel_asset(std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn linux_heel_asset_for_current_arch() -> Result<ManagedHeelAsset, ManagedHeelError> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(ManagedHeelAsset {
            release_asset_name: "heel-x86_64-unknown-linux-gnu",
            local_binary_name: HEEL_BINARY_FILE,
        }),
        "aarch64" => Ok(ManagedHeelAsset {
            release_asset_name: "heel-aarch64-unknown-linux-gnu",
            local_binary_name: HEEL_BINARY_FILE,
        }),
        arch => Err(ManagedHeelError::UnsupportedLinuxArch { arch }),
    }
}

pub(crate) async fn ensure_managed_heel_binary(
    install_root: &Path,
    asset: ManagedHeelAsset,
) -> Result<PathBuf, ManagedHeelError> {
    let binary_dir = install_root.join("bin");
    async_fs::create_dir_all(&binary_dir)
        .await
        .map_err(|source| ManagedHeelError::CreateDir {
            path: binary_dir.clone(),
            source,
        })?;

    let binary_path = binary_dir.join(asset.local_binary_name());
    let state_path = install_root.join(HEEL_RELEASE_STATE_FILE);

    // Fast path: if binary exists and was checked recently, skip the network call.
    if binary_path.exists() {
        if let Ok(Some(state)) = read_state(&state_path).await {
            if let Some(ref checked_at) = state.last_checked_at {
                if let Ok(ts) = checked_at.parse::<jiff::Timestamp>() {
                    let elapsed = jiff::Timestamp::now().duration_since(ts);
                    if elapsed
                        < CHECK_INTERVAL
                            .try_into()
                            .unwrap_or(jiff::SignedDuration::ZERO)
                    {
                        tracing::debug!(
                            binary = %binary_path.display(),
                            "skipping heel update check (checked recently)"
                        );
                        return Ok(binary_path);
                    }
                }
            }
        }
    }

    // The release lookup is an update check, not a correctness requirement:
    // when it fails (network down, GitHub rate limit) and a usable binary is
    // already installed, keep serving the installed binary.
    let release = match fetch_latest_release().await {
        Ok(release) => release,
        Err(error) => {
            if binary_present_and_nonempty(&binary_path).await? {
                tracing::warn!(
                    error = %error,
                    binary = %binary_path.display(),
                    "heel release check failed; using existing managed binary"
                );
                return Ok(binary_path);
            }
            return Err(error);
        }
    };
    let release_asset = release
        .assets
        .iter()
        .find(|candidate| candidate.name == asset.release_asset_name())
        .cloned()
        .ok_or_else(|| ManagedHeelError::MissingReleaseAsset {
            release_tag: release.tag_name.clone(),
            asset_name: asset.release_asset_name(),
        })?;

    if binary_is_current(
        &binary_path,
        &state_path,
        &release.tag_name,
        asset.release_asset_name(),
    )
    .await?
    {
        // Binary is up-to-date — just refresh the check timestamp
        let _ = write_state(
            &state_path,
            &ManagedHeelState {
                release_tag: release.tag_name,
                asset_name: asset.release_asset_name().to_string(),
                last_checked_at: Some(jiff::Timestamp::now().to_string()),
            },
        )
        .await;
        return Ok(binary_path);
    }

    tracing::info!(
        release_tag = %release.tag_name,
        asset = %release_asset.name,
        destination = %binary_path.display(),
        "downloading managed heel binary"
    );
    let bytes = download_asset_bytes(&release_asset).await?;
    install_binary(&binary_path, bytes.as_ref()).await?;
    write_state(
        &state_path,
        &ManagedHeelState {
            release_tag: release.tag_name,
            asset_name: release_asset.name,
            last_checked_at: Some(jiff::Timestamp::now().to_string()),
        },
    )
    .await?;

    Ok(binary_path)
}

async fn fetch_latest_release() -> Result<GithubRelease, ManagedHeelError> {
    let mut backend = zenwave::client();
    let mut request = backend
        .get(HEEL_RELEASE_API_URL)
        .map_err(ManagedHeelError::MetadataRequest)?
        .header("User-Agent", USER_AGENT)
        .map_err(ManagedHeelError::MetadataRequest)?;
    // An authenticated request has a far higher GitHub API rate limit.
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        request = request
            .header("Authorization", &format!("Bearer {token}"))
            .map_err(ManagedHeelError::MetadataRequest)?;
    }
    request
        .json::<GithubRelease>()
        .await
        .map_err(ManagedHeelError::MetadataDecode)
}

async fn download_asset_bytes(asset: &GithubAsset) -> Result<bytes::Bytes, ManagedHeelError> {
    let mut backend = zenwave::client();
    let bytes = backend
        .get(asset.browser_download_url.as_str())
        .map_err(|source| ManagedHeelError::AssetDownload {
            asset_name: asset.name.clone(),
            source,
        })?
        .header("User-Agent", USER_AGENT)
        .map_err(|source| ManagedHeelError::AssetDownload {
            asset_name: asset.name.clone(),
            source,
        })?
        .bytes()
        .await
        .map_err(|source| ManagedHeelError::AssetPayload {
            asset_name: asset.name.clone(),
            source,
        })?;
    if bytes.is_empty() {
        return Err(ManagedHeelError::EmptyAssetPayload {
            asset_name: asset.name.clone(),
        });
    }
    Ok(bytes)
}

async fn binary_is_current(
    binary_path: &Path,
    state_path: &Path,
    release_tag: &str,
    asset_name: &str,
) -> Result<bool, ManagedHeelError> {
    if !binary_present_and_nonempty(binary_path).await? {
        return Ok(false);
    }
    let Some(state) = read_state(state_path).await? else {
        return Ok(false);
    };
    Ok(state.release_tag == release_tag && state.asset_name == asset_name)
}

async fn binary_present_and_nonempty(binary_path: &Path) -> Result<bool, ManagedHeelError> {
    if !try_exists(binary_path, "check managed heel binary presence").await? {
        return Ok(false);
    }
    let metadata =
        async_fs::metadata(binary_path)
            .await
            .map_err(|source| ManagedHeelError::Io {
                operation: "read managed heel binary metadata",
                path: binary_path.to_path_buf(),
                source,
            })?;
    Ok(metadata.is_file() && metadata.len() > 0)
}

async fn read_state(path: &Path) -> Result<Option<ManagedHeelState>, ManagedHeelError> {
    match async_fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|source| {
            ManagedHeelError::StateParse {
                path: path.to_path_buf(),
                source,
            }
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ManagedHeelError::Io {
            operation: "read managed heel state",
            path: path.to_path_buf(),
            source,
        }),
    }
}

async fn write_state(path: &Path, state: &ManagedHeelState) -> Result<(), ManagedHeelError> {
    let bytes = serde_json::to_vec(state).map_err(|source| ManagedHeelError::StateSerialize {
        path: path.to_path_buf(),
        source,
    })?;
    async_fs::write(path, bytes)
        .await
        .map_err(|source| ManagedHeelError::Io {
            operation: "write managed heel state",
            path: path.to_path_buf(),
            source,
        })
}

async fn install_binary(binary_path: &Path, bytes: &[u8]) -> Result<(), ManagedHeelError> {
    let temporary_path = temporary_download_path(binary_path);
    async_fs::write(&temporary_path, bytes)
        .await
        .map_err(|source| ManagedHeelError::Io {
            operation: "write temporary managed heel binary",
            path: temporary_path.clone(),
            source,
        })?;
    make_executable(&temporary_path).await?;
    if try_exists(binary_path, "check existing managed heel binary presence").await? {
        async_fs::remove_file(binary_path)
            .await
            .map_err(|source| ManagedHeelError::Io {
                operation: "remove existing managed heel binary",
                path: binary_path.to_path_buf(),
                source,
            })?;
    }
    async_fs::rename(&temporary_path, binary_path)
        .await
        .map_err(|source| ManagedHeelError::Io {
            operation: "rename managed heel binary into place",
            path: binary_path.to_path_buf(),
            source,
        })
}

async fn try_exists(path: &Path, operation: &'static str) -> Result<bool, ManagedHeelError> {
    match async_fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ManagedHeelError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn temporary_download_path(binary_path: &Path) -> PathBuf {
    let file_name = binary_path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(HEEL_BINARY_FILE);
    binary_path.with_file_name(format!("{file_name}.download"))
}

#[cfg(unix)]
async fn make_executable(path: &Path) -> Result<(), ManagedHeelError> {
    use std::os::unix::fs::PermissionsExt;

    let path = path.to_path_buf();
    let blocking_path = path.clone();
    blocking::unblock(move || {
        let mut permissions = std::fs::metadata(&blocking_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&blocking_path, permissions)
    })
    .await
    .map_err(|source| ManagedHeelError::Io {
        operation: "mark managed heel binary executable",
        path,
        source,
    })
}

#[cfg(not(unix))]
async fn make_executable(_path: &Path) -> Result<(), ManagedHeelError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_heel_asset_covers_every_published_target() {
        for (os, arch, asset_name, binary_name) in [
            ("macos", "aarch64", "heel-aarch64-apple-darwin", "heel"),
            ("macos", "x86_64", "heel-x86_64-apple-darwin", "heel"),
            ("linux", "aarch64", "heel-aarch64-unknown-linux-gnu", "heel"),
            ("linux", "x86_64", "heel-x86_64-unknown-linux-gnu", "heel"),
            (
                "windows",
                "x86_64",
                "heel-x86_64-pc-windows-msvc.exe",
                "heel.exe",
            ),
            (
                "windows",
                "aarch64",
                "heel-aarch64-pc-windows-msvc.exe",
                "heel.exe",
            ),
        ] {
            let asset = host_heel_asset(os, arch)
                .unwrap_or_else(|error| panic!("{os}/{arch} should be supported: {error}"));
            assert_eq!(asset.release_asset_name(), asset_name);
            assert_eq!(asset.local_binary_name(), binary_name);
        }
        assert!(matches!(
            host_heel_asset("freebsd", "x86_64"),
            Err(ManagedHeelError::UnsupportedHostTarget {
                os: "freebsd",
                arch: "x86_64"
            })
        ));
    }

    #[test]
    fn current_host_asset_matches_supported_runtime() {
        let asset = current_host_heel_asset().expect("supported test host");
        assert!(asset.release_asset_name().starts_with("heel-"));
        assert!(asset.local_binary_name().starts_with("heel"));
        assert!(
            asset
                .local_binary_name()
                .ends_with(std::env::consts::EXE_SUFFIX)
        );
    }

    #[test]
    fn current_arch_maps_to_linux_release_asset_name() {
        let asset = linux_heel_asset_for_current_arch().expect("supported test arch");
        assert!(asset.release_asset_name().starts_with("heel-"));
        assert!(asset.release_asset_name().ends_with("-unknown-linux-gnu"));
        assert_eq!(asset.local_binary_name(), HEEL_BINARY_FILE);
    }

    #[test]
    fn managed_state_round_trips() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_path = temp.path().join(HEEL_RELEASE_STATE_FILE);
        let state = ManagedHeelState {
            release_tag: "v1.2.3".to_string(),
            asset_name: "heel-x86_64-unknown-linux-gnu".to_string(),
            last_checked_at: Some("2026-04-13T00:00:00Z".to_string()),
        };

        futures_lite::future::block_on(async {
            write_state(&state_path, &state).await.expect("write state");
            let parsed = read_state(&state_path)
                .await
                .expect("read state")
                .expect("state exists");
            assert_eq!(parsed, state);
        });
    }
}
