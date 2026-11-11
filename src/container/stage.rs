//! Staging for the published workspace image's generated build-context entries.

use std::path::{Path, PathBuf};

use crate::tools::{HostProgram, ToolTarget};

use crate::tools::embedded_distribution;

/// Errors staging the workspace image's generated context can hit.
#[derive(Debug, thiserror::Error)]
pub enum StageContextError {
    /// The target architecture is not one the image build supports.
    #[error("unsupported architecture {0} (expected amd64 or arm64)")]
    UnsupportedArch(String),
    /// Baseline tool or `heel` loading or staging failed.
    #[error("tool staging failed: {0}")]
    Tools(#[source] crate::tools::ToolRuntimeError),
    /// The distribution's `heel` could not be copied into the context.
    #[error("staging the heel binary failed: {0}")]
    Heel(#[source] std::io::Error),
    /// The built `tasks` binary could not be copied into the context.
    #[error("staging the tasks binary failed: {0}")]
    Tasks(#[source] std::io::Error),
}

/// Stages the workspace context's generated entries for a Linux architecture.
///
/// Writes the baseline tools under `<context>/tools/<cache key>/bin` (the
/// layout the image's runtime tool verification expects), the distribution's
/// Linux `heel` binary at `<context>/heel`, pinned at the `heel` library
/// version this binary links, and `tasks_binary`, the `tasks` command built
/// for the same architecture, at `<context>/tasks`.
///
/// `scratch` receives the transient tool artifact cache; it is safe to
/// discard after staging.
///
/// # Errors
///
/// Returns [`StageContextError`] when the architecture is unsupported, or
/// tool artifacts or `heel` fail to load or stage.
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
    let distribution = embedded_distribution().map_err(StageContextError::Tools)?;
    let staged = distribution
        .stage_image_tools(&target, &scratch.join("artifacts"), &context.join("tools"))
        .await
        .map_err(StageContextError::Tools)?;
    // The staging machinery leaves a `.<key>.lock` sibling; the image context
    // should carry only the staged tree.
    if let Some(key) = staged.file_name() {
        let lock = staged.with_file_name(format!(".{}.lock", key.to_string_lossy()));
        let _ = async_fs::remove_file(lock).await;
    }

    let heel = distribution
        .prepare_host_program(HostProgram::Heel, &target, scratch)
        .await
        .map_err(StageContextError::Tools)?;
    async_fs::copy(heel.path(), context.join("heel"))
        .await
        .map_err(StageContextError::Heel)?;

    // The `tasks` command the image puts on its `PATH`; the caller built it
    // for the image's architecture.
    async_fs::copy(tasks_binary, context.join("tasks"))
        .await
        .map_err(StageContextError::Tasks)?;

    Ok(staged)
}
