//! Bundled resources extraction.
//!
//! Skills and subagents are embedded in the binary and extracted
//! to the canonical May home on first run.
//! Local/heel session sandboxes get symlinks pointing to their home-scoped resources.
//! Container sessions use mount points managed by container lifecycle code.

use std::path::Path;

use async_fs;

use include_dir::{Dir, include_dir};

use crate::session::SessionPaths;

// Embed the skills and subagents directories at compile time
static SKILLS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/skills");
static SUBAGENTS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/bundle/subagents");

// Embed container build context managed by May.
static CONTAINER_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/container");

/// Bundle extraction error.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Bundled resources manager.
#[derive(Debug)]
pub struct Bundle;

impl Bundle {
    /// Extract bundled resources to ~/.may/ if not already present.
    /// Creates ~/.may/skills/, ~/.may/subagents/, and ~/.may/container/ directories.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when a bundled resource directory or file
    /// cannot be extracted under `may_dir`.
    pub async fn ensure_extracted(may_dir: &Path) -> Result<(), BundleError> {
        let skills_dir = may_dir.join("skills");
        let subagents_dir = may_dir.join("subagents");
        let container_dir = may_dir.join("container");
        let heel_dir = container_dir.join("heel");
        let heel_bin_dir = heel_dir.join("bin");

        Self::ensure_materialized_dir(&skills_dir).await?;
        Self::ensure_materialized_dir(&subagents_dir).await?;
        Self::ensure_materialized_dir(&container_dir).await?;
        Self::ensure_materialized_dir(&heel_dir).await?;
        Self::remove_legacy_heel_source_tree(&heel_dir).await?;
        async_fs::create_dir_all(&heel_bin_dir).await?;

        // Built-in resources must stay in lockstep with the running binary.
        // Always overwrite bundled content to avoid stale skills/subagents after upgrades.
        Self::extract_dir_overwrite(&SKILLS_DIR, &skills_dir).await?;

        Self::extract_dir_overwrite(&SUBAGENTS_DIR, &subagents_dir).await?;

        // Extract container build context. Managed files are always overwritten since they
        // are owned by the running May binary rather than user content.
        Self::extract_dir_overwrite(&CONTAINER_DIR, &container_dir).await?;

        Ok(())
    }

    /// Get the container build context directory path.
    #[must_use]
    pub fn container_context_dir(may_dir: &Path) -> std::path::PathBuf {
        may_dir.join("container")
    }

    /// Create symlinks in a local/heel session sandbox pointing to its home-scoped resources.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when a symlink or its parent directory cannot
    /// be created.
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
    async fn extract_dir_overwrite(
        src: &'static Dir<'static>,
        dest: &Path,
    ) -> Result<(), BundleError> {
        let mut pending = vec![src];
        while let Some(dir) = pending.pop() {
            for file in dir.files() {
                let file_path = dest.join(file.path());
                if let Some(parent) = file_path.parent() {
                    async_fs::create_dir_all(parent).await?;
                }
                async_fs::write(&file_path, file.contents()).await?;
            }
            pending.extend(dir.dirs());
        }
        Ok(())
    }

    async fn ensure_materialized_dir(path: &Path) -> Result<(), BundleError> {
        match async_fs::symlink_metadata(path).await {
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
                Err(BundleError::Io(std::io::Error::other(format!(
                    "expected directory at {}, found non-directory entry",
                    path.display()
                ))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                async_fs::create_dir_all(path).await?;
                Ok(())
            }
            Err(error) => Err(BundleError::Io(error)),
        }
    }

    async fn remove_legacy_heel_source_tree(heel_dir: &Path) -> Result<(), BundleError> {
        for legacy_entry in [
            "Cargo.lock",
            "Cargo.toml",
            "cli",
            "nodejs",
            "src",
            "templates",
        ] {
            Self::remove_path_if_exists(&heel_dir.join(legacy_entry)).await?;
        }
        Ok(())
    }

    async fn remove_path_if_exists(path: &Path) -> Result<(), BundleError> {
        match async_fs::symlink_metadata(path).await {
            Ok(metadata) if metadata.is_dir() => {
                async_fs::remove_dir_all(path).await?;
                Ok(())
            }
            Ok(_) => {
                async_fs::remove_file(path).await?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BundleError::Io(error)),
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
