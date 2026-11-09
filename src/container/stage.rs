//! Staging for the published workspace image's generated build-context entries.

use std::path::{Path, PathBuf};

use aither_sandbox::ToolTarget;

use crate::heel::release::{
    HEEL_RELEASE_TAG, HEEL_RELEASES_API_URL, ManagedHeelInstall, ensure_managed_heel_binary,
    host_heel_asset,
};
use crate::tools::embedded_distribution;

/// Errors staging the workspace image's generated context can hit.
#[derive(Debug, thiserror::Error)]
pub enum StageContextError {
    /// The target architecture is not one the image build supports.
    #[error("unsupported architecture {0} (expected amd64 or arm64)")]
    UnsupportedArch(String),
    /// Baseline tool loading or staging failed.
    #[error("baseline tool staging failed: {0}")]
    Tools(#[source] aither_sandbox::ToolRuntimeError),
    /// Managed heel download or verification failed.
    #[error("heel release provisioning failed: {0}")]
    Heel(String),
    /// The built `tasks` binary could not be copied into the context.
    #[error("staging the tasks binary failed: {0}")]
    Tasks(#[source] std::io::Error),
}

/// Stages the workspace context's generated entries for a Linux architecture.
///
/// Writes the baseline tools under `<context>/tools/<cache key>/bin` (the
/// layout the image's runtime tool verification expects) and the managed
/// Linux `heel` binary at `<context>/heel`, pinned to the `heel` library
/// version this binary is built against, and `tasks_binary`, the `tasks`
/// command built for the same architecture, at `<context>/tasks`.
///
/// `scratch` receives the transient tool artifact cache and heel release
/// state; both are safe to discard after staging.
///
/// # Errors
///
/// Returns [`StageContextError`] when the architecture is unsupported, tool
/// artifacts fail to load or stage, or the heel release download fails.
pub async fn stage_workspace_context(
    arch: &str,
    context: &Path,
    scratch: &Path,
    tasks_binary: &Path,
) -> Result<PathBuf, StageContextError> {
    let arch: &'static str = match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => return Err(StageContextError::UnsupportedArch(other.to_string())),
    };

    let target = ToolTarget::from_probe("linux", arch).map_err(StageContextError::Tools)?;
    let staged = embedded_distribution()
        .map_err(StageContextError::Tools)?
        .stage_image_tools(&target, &scratch.join("artifacts"), &context.join("tools"))
        .await
        .map_err(StageContextError::Tools)?;
    // The staging machinery leaves a `.<key>.lock` sibling; the image context
    // should carry only the staged tree.
    if let Some(key) = staged.file_name() {
        let lock = staged.with_file_name(format!(".{}.lock", key.to_string_lossy()));
        let _ = async_fs::remove_file(lock).await;
    }

    let install = ManagedHeelInstall {
        binary_dir: context.to_path_buf(),
        state_path: scratch.join("heel-release-state.json"),
    };
    ensure_managed_heel_binary(
        &install,
        host_heel_asset("linux", arch)
            .map_err(|error| StageContextError::Heel(error.to_string()))?,
        HEEL_RELEASE_TAG,
        HEEL_RELEASES_API_URL,
    )
    .await
    .map_err(|error| StageContextError::Heel(error.to_string()))?;

    // The `tasks` command the image puts on its `PATH`; the caller built it
    // for the image's architecture.
    async_fs::copy(tasks_binary, context.join("tasks"))
        .await
        .map_err(StageContextError::Tasks)?;

    Ok(staged)
}
