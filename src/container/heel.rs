//! Managed `heel` binary provisioning for container build contexts.

use std::path::Path;

use super::ContainerError;
use crate::heel::release::{
    HEEL_RELEASE_STATE_FILE, HEEL_RELEASE_TAG, HEEL_RELEASES_API_URL, ManagedHeelInstall,
    ensure_managed_heel_binary, linux_heel_asset_for_current_arch,
};

/// The container heel layout: the binary inside the Docker build context at
/// `container/heel/bin` (the Dockerfile copies `heel/bin/heel`) and the
/// release state outside it at `state/container-heel-release.json`, so the
/// timestamp rewrite on every release check never perturbs the context hash
/// that names the workspace image.
fn container_heel_install(may_dir: &Path) -> ManagedHeelInstall {
    ManagedHeelInstall {
        binary_dir: may_dir.join("container").join("heel").join("bin"),
        state_path: may_dir.join("state").join("container-heel-release.json"),
    }
}

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

    // Older versions wrote the release state beside the binary inside the
    // build context; drop the stale copy so it cannot perturb the context
    // hash.
    let stale_state = may_dir
        .join("container")
        .join("heel")
        .join(HEEL_RELEASE_STATE_FILE);
    if let Err(error) = async_fs::remove_file(&stale_state).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }

    ensure_managed_heel_binary(
        &container_heel_install(may_dir),
        asset,
        HEEL_RELEASE_TAG,
        HEEL_RELEASES_API_URL,
    )
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

    #[test]
    fn container_install_keeps_state_outside_the_build_context() {
        let may_dir = Path::new("/may");
        let install = container_heel_install(may_dir);

        assert_eq!(
            install.binary_dir,
            may_dir.join("container").join("heel").join("bin"),
            "the Dockerfile copies heel/bin/heel from the build context"
        );
        assert!(
            !install.state_path.starts_with(may_dir.join("container")),
            "release state must live outside the hashed build context"
        );
        assert_eq!(
            install.state_path,
            may_dir.join("state").join("container-heel-release.json")
        );
    }
}
