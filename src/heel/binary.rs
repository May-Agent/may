use std::path::PathBuf;

use super::release::{ManagedHeelError, current_host_heel_asset, ensure_managed_heel_binary};
use crate::config::MayConfig;

#[derive(Debug, thiserror::Error)]
enum HeelBinaryConfigurationError {
    #[error("failed to resolve May directory: {0}")]
    MayDir(#[from] crate::config::ConfigError),
    #[error("failed to resolve managed heel binary: {0}")]
    ManagedHeel(#[from] ManagedHeelError),
}

pub async fn configure_heel_binary() {
    if std::env::var_os("HEEL_BIN").is_some() {
        return;
    }

    let binary_name = heel_binary_name();
    if search_path_for_binary(&binary_name).is_some() {
        return;
    }

    match resolve_heel_binary().await {
        Ok(path) => {
            // SAFETY: this runs during process startup before May creates background tasks.
            unsafe { std::env::set_var("HEEL_BIN", &path) };
            tracing::info!(heel_bin = %path.display(), "configured HEEL_BIN for May runtime");
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to preconfigure HEEL_BIN");
        }
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
