//! Container image management for May workspace environments.

use std::path::{Path, PathBuf};

use aither_sandbox::ContainerImageSpec;
use blocking::unblock;
use bollard::query_parameters::{BuildImageOptionsBuilder, ListImagesOptionsBuilder};
use futures_lite::StreamExt;

use super::{ContainerError, ContainerRuntime};

/// Default image tag for the May workspace container.
pub const MAY_IMAGE_TAG: &str = "may-workspace:latest";

/// Stage of an image ensure/build operation reported via [`ImageBuildProgress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageBuildPhase {
    /// Checking whether the image already exists locally.
    CheckingExisting,
    /// The image already exists; no build is needed.
    AlreadyExists,
    /// A build is starting.
    BuildStarting,
    /// A line of build output was produced.
    BuildOutput,
    /// The build finished successfully.
    BuildCompleted,
}

/// A progress update emitted while ensuring or building a container image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuildProgress {
    /// Stage this update belongs to.
    pub phase: ImageBuildPhase,
    /// Human-readable description of the update.
    pub message: String,
}

impl ImageBuildProgress {
    fn new(phase: ImageBuildPhase, message: impl Into<String>) -> Self {
        Self {
            phase,
            message: message.into(),
        }
    }
}

/// Build the default image spec for the bundled May workspace image.
#[must_use]
pub fn default_image_spec(context_dir: impl Into<PathBuf>) -> ContainerImageSpec {
    ContainerImageSpec::new(MAY_IMAGE_TAG, context_dir)
}

/// Check if an image already exists.
///
/// # Errors
///
/// Returns [`ContainerError::Docker`] when the image list query fails.
pub async fn image_exists(
    runtime: &ContainerRuntime,
    spec: &ContainerImageSpec,
) -> Result<bool, ContainerError> {
    let filters: std::collections::HashMap<String, Vec<String>> =
        [("reference".to_string(), vec![spec.tag.clone()])].into();
    let opts = ListImagesOptionsBuilder::default()
        .filters(&filters)
        .build();
    let images = runtime.client.list_images(Some(opts)).await?;
    Ok(!images.is_empty())
}

/// Build a container image from a Docker build context.
///
/// # Errors
///
/// Returns [`ContainerError`] when the build fails; see
/// [`build_image_with_progress`].
pub async fn build_image(
    runtime: &ContainerRuntime,
    spec: &ContainerImageSpec,
) -> Result<(), ContainerError> {
    build_image_with_progress(runtime, spec, |_| {}).await
}

/// Build a container image and stream progress messages to callback.
///
/// # Errors
///
/// Returns [`ContainerError::Io`] when the build context cannot be archived or
/// the Dockerfile lies outside the context directory,
/// [`ContainerError::Docker`] when the daemon rejects the build request, or
/// [`ContainerError::ImageBuildFailed`] when the build itself reports an error.
pub async fn build_image_with_progress<F>(
    runtime: &ContainerRuntime,
    spec: &ContainerImageSpec,
    mut on_progress: F,
) -> Result<(), ContainerError>
where
    F: FnMut(ImageBuildProgress),
{
    tracing::info!(
        context = %spec.context_dir.display(),
        tag = %spec.tag,
        dockerfile = ?spec.dockerfile,
        "Building container image"
    );
    on_progress(ImageBuildProgress::new(
        ImageBuildPhase::BuildStarting,
        format!("building image {}", spec.tag),
    ));

    let context_dir = spec.context_dir.clone();
    let tar_bytes = unblock(move || create_tar_context(&context_dir)).await?;

    let mut opts_builder = BuildImageOptionsBuilder::default().t(&spec.tag).rm(true);

    if let Some(ref dockerfile) = spec.dockerfile {
        let relative = dockerfile_relative_to_context(dockerfile, &spec.context_dir).await?;
        opts_builder = opts_builder.dockerfile(&relative);
    }

    let opts = opts_builder.build();
    let body = bollard::body_full(tar_bytes.into());
    let mut stream = runtime.client.build_image(opts, None, Some(body));

    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(stream_text) = &info.stream {
                    let trimmed = stream_text.trim();
                    if !trimmed.is_empty() {
                        tracing::debug!(target: "container::build", "{trimmed}");
                        on_progress(ImageBuildProgress::new(
                            ImageBuildPhase::BuildOutput,
                            trimmed.to_string(),
                        ));
                    }
                }
                if let Some(ref detail) = info.error_detail {
                    let msg = detail.message.as_deref().unwrap_or("unknown build error");
                    return Err(ContainerError::ImageBuildFailed(msg.to_string()));
                }
            }
            Err(e) => return Err(ContainerError::Docker(e)),
        }
    }

    tracing::info!(tag = %spec.tag, "Container image built successfully");
    on_progress(ImageBuildProgress::new(
        ImageBuildPhase::BuildCompleted,
        format!("image {} is ready", spec.tag),
    ));
    Ok(())
}

/// Ensure the requested container image exists, building it if needed.
///
/// # Errors
///
/// Returns [`ContainerError`] when checking for the image or building it
/// fails; see [`ensure_image_with_progress`].
pub async fn ensure_image(
    runtime: &ContainerRuntime,
    spec: &ContainerImageSpec,
) -> Result<(), ContainerError> {
    ensure_image_with_progress(runtime, spec, |_| {}).await
}

/// Ensure image exists and stream progress messages to callback.
///
/// # Errors
///
/// Returns [`ContainerError::Docker`] when the image query fails, or the
/// errors of [`build_image_with_progress`] when a build is needed.
pub async fn ensure_image_with_progress<F>(
    runtime: &ContainerRuntime,
    spec: &ContainerImageSpec,
    mut on_progress: F,
) -> Result<(), ContainerError>
where
    F: FnMut(ImageBuildProgress),
{
    on_progress(ImageBuildProgress::new(
        ImageBuildPhase::CheckingExisting,
        format!("checking image {}", spec.tag),
    ));
    if image_exists(runtime, spec).await? {
        tracing::debug!(tag = %spec.tag, "Container image already exists");
        on_progress(ImageBuildProgress::new(
            ImageBuildPhase::AlreadyExists,
            format!("image {} already exists", spec.tag),
        ));
        return Ok(());
    }

    build_image_with_progress(runtime, spec, on_progress).await
}

/// Resolve `dockerfile` to the forward-slash path inside the build context
/// that the build API expects, whatever the host's separator is.
async fn dockerfile_relative_to_context(
    dockerfile: &Path,
    context_dir: &Path,
) -> Result<String, ContainerError> {
    let (context, dockerfile_path) = futures_lite::future::zip(
        async_fs::canonicalize(context_dir),
        async_fs::canonicalize(dockerfile),
    )
    .await;
    let (context, dockerfile_path) = (context?, dockerfile_path?);
    let relative = dockerfile_path.strip_prefix(&context).map_err(|_| {
        ContainerError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "dockerfile {} must be inside build context {}",
                dockerfile_path.display(),
                context.display()
            ),
        ))
    })?;

    let segments = relative
        .components()
        .map(|component| {
            component.as_os_str().to_str().ok_or_else(|| {
                ContainerError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("dockerfile path {} is not UTF-8", relative.display()),
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(segments.join("/"))
}

/// Create a tar archive from a directory for the Docker build context.
fn create_tar_context(context_dir: &Path) -> Result<Vec<u8>, ContainerError> {
    let buf = Vec::new();
    let mut archive = tar::Builder::new(buf);
    archive.append_dir_all(".", context_dir)?;
    archive.into_inner().map_err(|e| {
        ContainerError::Io(std::io::Error::other(format!(
            "Failed to finalize tar archive: {e}"
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nested_dockerfile_is_a_forward_slash_path_inside_the_context() {
        let context = tempfile::TempDir::new().expect("context dir");
        let nested = context.path().join("images").join("dev");
        std::fs::create_dir_all(&nested).expect("nested dir");
        let dockerfile = nested.join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").expect("dockerfile");

        let relative = dockerfile_relative_to_context(&dockerfile, context.path())
            .await
            .expect("dockerfile inside context");
        assert_eq!(relative, "images/dev/Dockerfile");
    }

    #[tokio::test]
    async fn dockerfile_outside_the_context_is_rejected() {
        let context = tempfile::TempDir::new().expect("context dir");
        let outside = tempfile::TempDir::new().expect("outside dir");
        let dockerfile = outside.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\n").expect("dockerfile");

        let error = dockerfile_relative_to_context(&dockerfile, context.path())
            .await
            .expect_err("dockerfile outside context must be rejected");
        assert!(
            error.to_string().contains("must be inside build context"),
            "unexpected error: {error}"
        );
    }
}
