//! Container image management for May workspace environments.

use bollard::query_parameters::{CreateImageOptionsBuilder, ListImagesOptionsBuilder};
use futures_lite::StreamExt;

use super::{ContainerError, ContainerRuntime};

/// The GHCR reference of the workspace image May runs containers from.
///
/// Derived at build time from the content hash of everything the image
/// carries, so the binary always names exactly the image CI published for it.
pub const MAY_WORKSPACE_IMAGE: &str = env!("MAY_WORKSPACE_IMAGE");

/// Stage of an image ensure/pull operation reported via [`ImagePullProgress`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagePullPhase {
    /// Checking whether the image already exists locally.
    CheckingExisting,
    /// The image already exists; no pull is needed.
    AlreadyExists,
    /// A pull is starting.
    PullStarting,
    /// A line of pull output was produced.
    PullProgress,
    /// The pull finished successfully.
    PullCompleted,
}

/// A progress update emitted while ensuring or pulling a container image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePullProgress {
    /// Stage this update belongs to.
    pub phase: ImagePullPhase,
    /// Human-readable description of the update.
    pub message: String,
}

impl ImagePullProgress {
    fn new(phase: ImagePullPhase, message: impl Into<String>) -> Self {
        Self {
            phase,
            message: message.into(),
        }
    }
}

/// Check if an image already exists.
///
/// # Errors
///
/// Returns [`ContainerError::Docker`] when the image list query fails.
pub async fn image_exists(runtime: &ContainerRuntime, image: &str) -> Result<bool, ContainerError> {
    let filters: std::collections::HashMap<String, Vec<String>> =
        [("reference".to_string(), vec![image.to_string()])].into();
    let opts = ListImagesOptionsBuilder::default()
        .filters(&filters)
        .build();
    let images = runtime.client.list_images(Some(opts)).await?;
    Ok(!images.is_empty())
}

/// Pull a container image and stream progress messages to callback.
///
/// # Errors
///
/// Returns [`ContainerError::Docker`] when the daemon rejects or fails the
/// pull request.
pub async fn pull_image_with_progress<F>(
    runtime: &ContainerRuntime,
    image: &str,
    mut on_progress: F,
) -> Result<(), ContainerError>
where
    F: FnMut(ImagePullProgress),
{
    tracing::info!(image, "Pulling container image");
    on_progress(ImagePullProgress::new(
        ImagePullPhase::PullStarting,
        format!("pulling image {image}"),
    ));

    let opts = CreateImageOptionsBuilder::default()
        .from_image(image)
        .build();
    let mut stream = runtime.client.create_image(Some(opts), None, None);

    while let Some(result) = stream.next().await {
        let info = result?;
        let Some(status) = info.status.as_deref() else {
            continue;
        };
        let progress =
            info.progress_detail
                .as_ref()
                .and_then(|detail| match (detail.current, detail.total) {
                    (Some(current), Some(total)) if total > 0 => Some(format!("{current}/{total}")),
                    _ => None,
                });
        let message = match (&info.id, progress) {
            (Some(id), Some(progress)) => format!("{id}: {status} {progress}"),
            (Some(id), None) => format!("{id}: {status}"),
            _ => status.to_string(),
        };
        if !message.is_empty() {
            tracing::debug!(target: "container::pull", "{message}");
            on_progress(ImagePullProgress::new(
                ImagePullPhase::PullProgress,
                message,
            ));
        }
    }

    tracing::info!(image, "Container image pulled successfully");
    on_progress(ImagePullProgress::new(
        ImagePullPhase::PullCompleted,
        format!("image {image} is ready"),
    ));
    Ok(())
}

/// Ensure the requested container image exists, pulling it if needed.
///
/// # Errors
///
/// Returns [`ContainerError`] when checking for the image or pulling it
/// fails; see [`ensure_image_with_progress`].
pub async fn ensure_image(runtime: &ContainerRuntime, image: &str) -> Result<(), ContainerError> {
    ensure_image_with_progress(runtime, image, |_| {}).await
}

/// Ensure image exists and stream progress messages to callback.
///
/// # Errors
///
/// Returns [`ContainerError::Docker`] when the image query fails, or the
/// errors of [`pull_image_with_progress`] when a pull is needed.
pub async fn ensure_image_with_progress<F>(
    runtime: &ContainerRuntime,
    image: &str,
    mut on_progress: F,
) -> Result<(), ContainerError>
where
    F: FnMut(ImagePullProgress),
{
    on_progress(ImagePullProgress::new(
        ImagePullPhase::CheckingExisting,
        format!("checking image {image}"),
    ));
    if image_exists(runtime, image).await? {
        tracing::debug!(image, "Container image already exists");
        on_progress(ImagePullProgress::new(
            ImagePullPhase::AlreadyExists,
            format!("image {image} already exists"),
        ));
        return Ok(());
    }

    pull_image_with_progress(runtime, image, on_progress).await
}
