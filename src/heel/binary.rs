use std::path::{Path, PathBuf};

use super::release::{
    HEEL_RELEASE_TAG, HEEL_RELEASES_API_URL, ManagedHeelError, ManagedHeelInstall,
    current_host_heel_asset, ensure_managed_heel_binary,
};
use crate::config::MayConfig;

#[derive(Debug, thiserror::Error)]
enum HeelBinaryConfigurationError {
    #[error("failed to resolve May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    #[error("failed to resolve managed heel binary: {0}")]
    ManagedHeel(#[from] ManagedHeelError),
}

/// Resolve the heel binary for sandbox execution: honor `HEEL_BIN` when
/// set, otherwise use the managed download.
///
///
pub async fn configure_heel_binary() {
    if let Some(path) = std::env::var_os("HEEL_BIN") {
        let path = PathBuf::from(path);
        assert_heel_executes(&path, "HEEL_BIN environment variable").await;
        warn_on_version_mismatch(&path, "HEEL_BIN").await;
        return;
    }

    let binary_name = heel_binary_name();
    if let Some(path) = search_path_for_binary(&binary_name) {
        assert_heel_executes(&path, "PATH lookup").await;
        if heel_binary_version(&path).await.as_deref() == Some(required_heel_version()) {
            return;
        }
        tracing::warn!(
            heel_bin = %path.display(),
            required = required_heel_version(),
            "heel binary on PATH does not match the sandbox shim version; using the managed binary"
        );
    }

    match resolve_heel_binary().await {
        Ok(path) => {
            assert_heel_executes(&path, "managed download").await;
            // SAFETY: this runs during process startup before May creates background tasks.
            unsafe { std::env::set_var("HEEL_BIN", &path) };
            tracing::info!(heel_bin = %path.display(), "configured HEEL_BIN for May runtime");
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to preconfigure HEEL_BIN");
        }
    }
}

/// Verify the heel binary actually runs on this host.
///
/// A present-but-unrunnable binary (wrong architecture, wrong OS, truncated
/// download) would otherwise surface as confusing exec failures on every
/// sandboxed command, so fail loudly at startup instead.
async fn assert_heel_executes(path: &Path, source: &str) {
    let command_path = path.to_path_buf();
    let output = blocking::unblock(move || {
        std::process::Command::new(&command_path)
            .arg("--version")
            .output()
    })
    .await;

    match output {
        Ok(output) if output.status.success() => {}
        Ok(output) => panic!(
            "heel binary at {} (from {source}) failed to report its version: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => panic!(
            "heel binary at {} (from {source}) cannot execute on this host: {error}",
            path.display()
        ),
    }
}

fn heel_binary_name() -> String {
    format!("heel{}", std::env::consts::EXE_SUFFIX)
}

/// Version the `heel` binary must report so it can run the IPC shims the
/// compiled-in `heel` library generates.
const fn required_heel_version() -> &'static str {
    env!("MAY_HEEL_LIB_VERSION")
}

/// `heel --version` output parsed into its version component, if readable.
async fn heel_binary_version(path: &Path) -> Option<String> {
    let command_path = path.to_path_buf();
    let output = blocking::unblock(move || {
        std::process::Command::new(&command_path)
            .arg("--version")
            .output()
    })
    .await
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().nth(1).map(str::to_string)
}

/// An explicit `HEEL_BIN` override stays honored on a version mismatch — the
/// user pointed at it on purpose — but the mismatch is logged because sandbox
/// IPC commands only work when the binary and the shim-generating library
/// share a version.
async fn warn_on_version_mismatch(path: &Path, source: &str) {
    let version = heel_binary_version(path).await;
    if version.as_deref() == Some(required_heel_version()) {
        return;
    }
    tracing::warn!(
        heel_bin = %path.display(),
        version = version.as_deref().unwrap_or("unknown"),
        required = required_heel_version(),
        "heel binary from {source} does not match the sandbox shim version; \
         IPC commands may fail"
    );
}

async fn resolve_heel_binary() -> Result<PathBuf, HeelBinaryConfigurationError> {
    let may_dir = MayConfig::may_dir()?;
    let asset = current_host_heel_asset()?;
    ensure_managed_heel_binary(
        &ManagedHeelInstall::under(&may_dir.join("heel").join("runtime")),
        asset,
        HEEL_RELEASE_TAG,
        HEEL_RELEASES_API_URL,
    )
    .await
    .map_err(HeelBinaryConfigurationError::from)
}

fn search_path_for_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
