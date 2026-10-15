use std::path::{Path, PathBuf};

use super::release::{ManagedHeelError, current_host_heel_asset, ensure_managed_heel_binary};
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
        assert_heel_executes(&PathBuf::from(path), "HEEL_BIN environment variable").await;
        return;
    }

    let binary_name = heel_binary_name();
    if let Some(path) = search_path_for_binary(&binary_name) {
        assert_heel_executes(&path, "PATH lookup").await;
        return;
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

async fn resolve_heel_binary() -> Result<PathBuf, HeelBinaryConfigurationError> {
    let may_dir = MayConfig::may_dir()?;
    let asset = current_host_heel_asset()?;
    ensure_managed_heel_binary(&may_dir.join("heel").join("runtime"), asset)
        .await
        .map_err(HeelBinaryConfigurationError::from)
}

fn search_path_for_binary(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}
