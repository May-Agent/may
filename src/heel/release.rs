use std::path::{Path, PathBuf};

use async_fs;
use serde::{Deserialize, Serialize};
use zenwave::Client;

/// GitHub release-metadata endpoint checked for the managed heel binary.
pub const HEEL_RELEASE_API_URL: &str = "https://api.github.com/repos/lexoliu/heel/releases/latest";
pub const HEEL_RELEASE_STATE_FILE: &str = "release-state.json";
const HEEL_BINARY_FILE: &str = "heel";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedHeelAsset {
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

/// Filesystem locations a managed heel install owns: the directory the
/// binary lands in and the release-check state file.
///
/// They are separate inputs because the binary may live inside a directory
/// whose contents are hashed — the container build context — while the
/// state file is rewritten on every release check and must not perturb
/// that hash.
#[derive(Debug, Clone)]
pub struct ManagedHeelInstall {
    /// Directory the managed binary is written into.
    pub binary_dir: PathBuf,
    /// Path of the JSON file recording the installed release.
    pub state_path: PathBuf,
}

impl ManagedHeelInstall {
    /// The single-root layout: `<root>/bin` and
    /// `<root>/release-state.json`.
    #[must_use]
    pub fn under(root: &Path) -> Self {
        Self {
            binary_dir: root.join("bin"),
            state_path: root.join(HEEL_RELEASE_STATE_FILE),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagedHeelError {
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
    last_checked_at: Option<jiff::Timestamp>,
}

/// How often to re-check the GitHub API for a new heel release (24 hours).
const CHECK_INTERVAL: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

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

pub fn current_host_heel_asset() -> Result<ManagedHeelAsset, ManagedHeelError> {
    host_heel_asset(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn linux_heel_asset_for_current_arch() -> Result<ManagedHeelAsset, ManagedHeelError> {
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

pub async fn ensure_managed_heel_binary(
    install: &ManagedHeelInstall,
    asset: ManagedHeelAsset,
    release_api_url: &str,
) -> Result<PathBuf, ManagedHeelError> {
    async_fs::create_dir_all(&install.binary_dir)
        .await
        .map_err(|source| ManagedHeelError::CreateDir {
            path: install.binary_dir.clone(),
            source,
        })?;
    let state_dir = install
        .state_path
        .parent()
        .expect("managed heel state path must name a file inside a directory");
    async_fs::create_dir_all(state_dir)
        .await
        .map_err(|source| ManagedHeelError::CreateDir {
            path: state_dir.to_path_buf(),
            source,
        })?;

    let binary_path = install.binary_dir.join(asset.local_binary_name());
    let state_path = &install.state_path;

    // Fast path: if binary exists and was checked recently, skip the network call.
    if binary_present_and_nonempty(&binary_path).await?
        && let Some(state) = read_state(state_path).await?
        && let Some(checked_at) = state.last_checked_at
        && jiff::Timestamp::now().duration_since(checked_at) < CHECK_INTERVAL
    {
        tracing::debug!(
            binary = %binary_path.display(),
            "skipping heel update check (checked recently)"
        );
        return Ok(binary_path);
    }

    let release = fetch_latest_release(release_api_url).await?;
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
        state_path,
        &release.tag_name,
        asset.release_asset_name(),
    )
    .await?
    {
        // Binary is up-to-date — just refresh the check timestamp
        write_state(
            state_path,
            &ManagedHeelState {
                release_tag: release.tag_name,
                asset_name: asset.release_asset_name().to_string(),
                last_checked_at: Some(jiff::Timestamp::now()),
            },
        )
        .await?;
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
        state_path,
        &ManagedHeelState {
            release_tag: release.tag_name,
            asset_name: release_asset.name,
            last_checked_at: Some(jiff::Timestamp::now()),
        },
    )
    .await?;

    Ok(binary_path)
}

async fn fetch_latest_release(release_api_url: &str) -> Result<GithubRelease, ManagedHeelError> {
    let mut backend = zenwave::client();
    let mut request = backend
        .get(release_api_url)
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
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    fn test_asset() -> ManagedHeelAsset {
        ManagedHeelAsset {
            release_asset_name: "heel-test-asset",
            local_binary_name: "heel",
        }
    }

    fn release_json(base_url: &str, tag: &str) -> Vec<u8> {
        serde_json::json!({
            "tag_name": tag,
            "assets": [{
                "name": "heel-test-asset",
                "browser_download_url": format!("{base_url}/heel-bin"),
            }],
        })
        .to_string()
        .into_bytes()
    }

    /// Binds a loopback port and immediately releases it, yielding a port that
    /// refuses connections — used to prove a code path never reaches the
    /// network.
    fn refused_release_api_url() -> String {
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|listener| listener.local_addr())
            .expect("reserve loopback port")
            .port();
        format!("http://127.0.0.1:{port}/release")
    }

    fn bind_loopback() -> (std::net::TcpListener, String) {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback listener");
        let base_url = format!(
            "http://{}",
            listener.local_addr().expect("loopback listener address")
        );
        (listener, base_url)
    }

    /// Serves canned `(status, body)` responses keyed by request path on the
    /// given listener. Returns a log of requested paths.
    fn serve_routes(
        listener: std::net::TcpListener,
        routes: Vec<(String, u16, Vec<u8>)>,
    ) -> Arc<Mutex<Vec<String>>> {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let requests_in_thread = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let routes = routes.clone();
                let requests = requests_in_thread.clone();
                std::thread::spawn(move || {
                    serve_connection(&mut stream, &routes, &requests);
                });
            }
        });
        requests
    }

    fn serve_connection(
        stream: &mut std::net::TcpStream,
        routes: &[(String, u16, Vec<u8>)],
        requests: &Mutex<Vec<String>>,
    ) {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("set read timeout");
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let Ok(read) = stream.read(&mut buffer) else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.len() > 64 * 1024 {
                return;
            }
        }
        let request_line = request
            .split(|byte| *byte == b'\n')
            .next()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .unwrap_or_default();
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();
        requests.lock().expect("request log").push(path.clone());
        let (status, body) = routes
            .iter()
            .find(|(route, ..)| *route == path)
            .map_or_else(
                || (404, b"not found".to_vec()),
                |(_, status, body)| (*status, body.clone()),
            );
        let reason = match status {
            200 => "OK",
            500 => "Internal Server Error",
            _ => "Not Found",
        };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).is_err() {
            return;
        }
        let _ = stream.write_all(&body);
    }

    fn install_binary_file(install_root: &Path, contents: &[u8]) -> PathBuf {
        let binary_dir = install_root.join("bin");
        std::fs::create_dir_all(&binary_dir).expect("create managed bin dir");
        let binary_path = binary_dir.join("heel");
        std::fs::write(&binary_path, contents).expect("write installed heel binary");
        binary_path
    }

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
            last_checked_at: Some(
                "2026-04-13T00:00:00Z"
                    .parse::<jiff::Timestamp>()
                    .expect("valid timestamp"),
            ),
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

    #[tokio::test]
    async fn failing_release_api_does_not_keep_installed_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install_root = temp.path();
        install_binary_file(install_root, b"stale-binary");

        let (listener, base_url) = bind_loopback();
        serve_routes(
            listener,
            vec![("/release".to_string(), 500, b"boom".to_vec())],
        );

        let install = ManagedHeelInstall::under(install_root);
        let error =
            ensure_managed_heel_binary(&install, test_asset(), &format!("{base_url}/release"))
                .await
                .expect_err("a failed release check must not fall back to the installed binary");
        assert!(
            matches!(
                error,
                ManagedHeelError::MetadataRequest(_) | ManagedHeelError::MetadataDecode(_)
            ),
            "expected the release-lookup error, got {error}"
        );
    }

    #[tokio::test]
    async fn unreachable_release_api_does_not_keep_installed_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install_root = temp.path();
        install_binary_file(install_root, b"stale-binary");

        let install = ManagedHeelInstall::under(install_root);
        ensure_managed_heel_binary(&install, test_asset(), &refused_release_api_url())
            .await
            .expect_err("a refused release check must not fall back to the installed binary");
    }

    #[tokio::test]
    async fn unrecorded_install_is_replaced_by_the_release_asset() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install_root = temp.path();
        install_binary_file(install_root, b"unrecorded-binary");
        // No release-state.json: the installed binary was never verified.

        let (listener, base_url) = bind_loopback();
        let requests = serve_routes(
            listener,
            vec![
                (
                    "/release".to_string(),
                    200,
                    release_json(&base_url, "v9.9.9"),
                ),
                ("/heel-bin".to_string(), 200, b"fresh-heel-binary".to_vec()),
            ],
        );

        let install = ManagedHeelInstall::under(install_root);
        let path =
            ensure_managed_heel_binary(&install, test_asset(), &format!("{base_url}/release"))
                .await
                .expect("unrecorded install must be replaced by the release asset");
        assert_eq!(
            std::fs::read(&path).expect("read installed binary"),
            b"fresh-heel-binary"
        );

        let state: serde_json::Value = serde_json::from_slice(
            &std::fs::read(install_root.join(HEEL_RELEASE_STATE_FILE)).expect("read state file"),
        )
        .expect("state file must be valid JSON");
        assert_eq!(state["release_tag"], "v9.9.9");
        assert_eq!(state["asset_name"], "heel-test-asset");
        assert!(state["last_checked_at"].is_string());

        let requested = requests.lock().expect("request log").clone();
        assert_eq!(
            requested,
            vec!["/release".to_string(), "/heel-bin".to_string()]
        );
    }

    #[tokio::test]
    async fn corrupt_state_file_is_an_error_naming_its_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install_root = temp.path();
        install_binary_file(install_root, b"installed-binary");

        for corrupt_state in [
            &b"{ not json"[..],
            &br#"{"release_tag":"v1","asset_name":"heel-test-asset","last_checked_at":"soon"}"#[..],
        ] {
            let state_path = install_root.join(HEEL_RELEASE_STATE_FILE);
            std::fs::write(&state_path, corrupt_state).expect("write corrupt state");

            // The API is unreachable: the corrupt state must fail before any
            // network fallback could mask it.
            let install = ManagedHeelInstall::under(install_root);
            let error =
                ensure_managed_heel_binary(&install, test_asset(), &refused_release_api_url())
                    .await
                    .expect_err("corrupt state must fail");
            match error {
                ManagedHeelError::StateParse { path, .. } => assert_eq!(path, state_path),
                other => panic!("expected a state-parse error, got {other}"),
            }
        }
    }

    #[tokio::test]
    async fn recently_checked_binary_skips_the_release_check() {
        let temp = tempfile::tempdir().expect("tempdir");
        let install_root = temp.path();
        let binary_path = install_binary_file(install_root, b"current-binary");
        write_state(
            &install_root.join(HEEL_RELEASE_STATE_FILE),
            &ManagedHeelState {
                release_tag: "v9.9.9".to_string(),
                asset_name: "heel-test-asset".to_string(),
                last_checked_at: Some(jiff::Timestamp::now()),
            },
        )
        .await
        .expect("write state");

        // The API is unreachable: success proves the fast path made no
        // network call.
        let install = ManagedHeelInstall::under(install_root);
        let path = ensure_managed_heel_binary(&install, test_asset(), &refused_release_api_url())
            .await
            .expect("recently checked binary must skip the release check");
        assert_eq!(path, binary_path);
    }

    #[tokio::test]
    async fn container_layout_writes_state_outside_the_build_context() {
        let temp = tempfile::tempdir().expect("tempdir");
        let may_dir = temp.path();
        let install = ManagedHeelInstall {
            binary_dir: may_dir.join("container").join("heel").join("bin"),
            state_path: may_dir.join("state").join("container-heel-release.json"),
        };

        let (listener, base_url) = bind_loopback();
        serve_routes(
            listener,
            vec![
                (
                    "/release".to_string(),
                    200,
                    release_json(&base_url, "v9.9.9"),
                ),
                ("/heel-bin".to_string(), 200, b"fresh-heel-binary".to_vec()),
            ],
        );

        let binary_path =
            ensure_managed_heel_binary(&install, test_asset(), &format!("{base_url}/release"))
                .await
                .expect("container heel install should succeed");
        assert_eq!(binary_path, install.binary_dir.join("heel"));

        let context_dir = may_dir.join("container");
        // The context hash covers every file beneath the build context, so
        // the state file must land outside it — under `state/` — with its
        // parent directory created on demand.
        assert!(
            install.state_path.is_file(),
            "release state should be written at the state path"
        );
        assert!(!install.state_path.starts_with(&context_dir));
        assert!(
            !context_dir
                .join("heel")
                .join(HEEL_RELEASE_STATE_FILE)
                .exists(),
            "no release state may remain inside the build context"
        );
    }
}
