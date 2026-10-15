//! Managed `heel` binary provisioning for container build contexts.

use std::path::Path;

use super::ContainerError;
use crate::heel::release::{ensure_managed_heel_binary, linux_heel_asset_for_current_arch};

/// Stage the managed `heel` binary under `may_dir` for container
/// environments.
///
/// # Errors
///
/// Returns [`ContainerError::Bootstrap`] when no release asset exists for
/// this architecture or staging the binary fails.
pub async fn ensure_container_heel_binary(may_dir: &Path) -> Result<(), ContainerError> {
    let asset = linux_heel_asset_for_current_arch()
        .map_err(|error| ContainerError::Bootstrap(error.to_string()))?;
    ensure_managed_heel_binary(&may_dir.join("container").join("heel"), asset)
        .await
        .map(|_| ())
        .map_err(|error| ContainerError::Bootstrap(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_arch_maps_to_linux_release_asset_name() {
        linux_heel_asset_for_current_arch().expect("supported test arch");
    }
}
