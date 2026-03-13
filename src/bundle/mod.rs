//! Bundled resources extraction.
//!
//! Skills and subagents are embedded in the binary and extracted
//! to the canonical May home on first run.
//! Local/leash session sandboxes get symlinks pointing to their home-scoped resources.
//! Container sessions use mount points managed by container lifecycle code.

use std::fs;
use std::path::Path;

use async_fs;

use include_dir::{Dir, include_dir};

use crate::session::SessionPaths;

// Embed the skills and subagents directories at compile time
static SKILLS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/skills");
static SUBAGENTS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/bundle/subagents");

// Embed container build context: Dockerfile + leash-ipc source
static CONTAINER_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/container");
static LEASH_IPC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../native-sandbox/ipc");

/// Bundle extraction error.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Bundled resources manager.
pub struct Bundle;

impl Bundle {
    /// Extract bundled resources to ~/.may/ if not already present.
    /// Creates ~/.may/skills/, ~/.may/subagents/, and ~/.may/container/ directories.
    pub fn ensure_extracted(may_dir: &Path) -> Result<(), BundleError> {
        let skills_dir = may_dir.join("skills");
        let subagents_dir = may_dir.join("subagents");
        let container_dir = may_dir.join("container");
        let leash_ipc_dir = container_dir.join("leash-ipc");

        Self::ensure_materialized_dir(&skills_dir)?;
        Self::ensure_materialized_dir(&subagents_dir)?;
        fs::create_dir_all(&container_dir)?;
        fs::create_dir_all(&leash_ipc_dir)?;

        // Built-in resources must stay in lockstep with the running binary.
        // Always overwrite bundled content to avoid stale skills/subagents after upgrades.
        Self::extract_dir_overwrite(&SKILLS_DIR, &skills_dir)?;

        Self::extract_dir_overwrite(&SUBAGENTS_DIR, &subagents_dir)?;

        // Extract container build context (Dockerfile + leash-ipc source).
        // Container files are always overwritten since they are managed by the binary,
        // not user-modifiable.
        Self::extract_dir_overwrite(&CONTAINER_DIR, &container_dir)?;
        Self::extract_dir_overwrite(&LEASH_IPC_DIR, &leash_ipc_dir)?;

        Ok(())
    }

    /// Get the container build context directory path.
    pub fn container_context_dir(may_dir: &Path) -> std::path::PathBuf {
        may_dir.join("container")
    }

    /// Create symlinks in a local/leash session sandbox pointing to its home-scoped resources.
    pub async fn create_sandbox_home_symlinks(
        sandbox_dir: &Path,
        home_root: &Path,
    ) -> Result<(), BundleError> {
        let session_paths = SessionPaths::new(sandbox_dir.to_path_buf());
        let subagents_target = home_root.join("subagents");
        let skills_target = home_root.join("skills");

        Self::create_dir_symlink(sandbox_dir, &subagents_target, "subagents").await?;
        Self::create_dir_symlink(sandbox_dir, &skills_target, "skills").await?;

        async_fs::create_dir_all(session_paths.tmp_dir()).await?;
        async_fs::create_dir_all(session_paths.data_dir()).await?;
        async_fs::create_dir_all(session_paths.files_dir()).await?;
        async_fs::create_dir_all(session_paths.images_dir()).await?;
        async_fs::create_dir_all(session_paths.outputs_dir()).await?;
        async_fs::create_dir_all(session_paths.artifacts_dir()).await?;

        Ok(())
    }

    /// Recursively extract an embedded directory, always overwriting existing files.
    fn extract_dir_overwrite(src: &Dir<'_>, dest: &Path) -> Result<(), BundleError> {
        Self::extract_dir_inner(src, dest, true)
    }

    fn extract_dir_inner(src: &Dir<'_>, dest: &Path, overwrite: bool) -> Result<(), BundleError> {
        for file in src.files() {
            let file_path = dest.join(file.path());
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            if overwrite || !file_path.exists() {
                fs::write(&file_path, file.contents())?;
            }
        }

        for dir in src.dirs() {
            Self::extract_dir_inner(dir, dest, overwrite)?;
        }

        Ok(())
    }

    fn ensure_materialized_dir(path: &Path) -> Result<(), BundleError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(BundleError::Io(std::io::Error::other(format!(
                        "symlinked bundle directory is no longer supported: {}",
                        path.display()
                    ))));
                }
                if metadata.is_dir() {
                    return Ok(());
                }
                return Err(BundleError::Io(std::io::Error::other(format!(
                    "expected directory at {}, found non-directory entry",
                    path.display()
                ))));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(path)?;
                return Ok(());
            }
            Err(error) => return Err(BundleError::Io(error)),
        }
    }

    /// Create directory symlink from parent/name to target.
    async fn create_dir_symlink(
        parent: &Path,
        target: &Path,
        name: &str,
    ) -> Result<(), BundleError> {
        let link = parent.join(name);

        // Skip if already correct
        if let Ok(metadata) = async_fs::symlink_metadata(&link).await
            && metadata.file_type().is_symlink()
        {
            if let Ok(existing_target) = async_fs::read_link(&link).await
                && existing_target == target
            {
                return Ok(());
            }
            let _ = async_fs::remove_file(&link).await;
        }

        #[cfg(unix)]
        {
            async_fs::unix::symlink(target, &link).await?;
        }

        #[cfg(windows)]
        {
            async_fs::windows::symlink_dir(target, &link).await?;
        }

        Ok(())
    }
}
