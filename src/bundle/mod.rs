//! Bundled resources extraction.
//!
//! Skills and subagents are embedded in the binary and extracted
//! to ~/.may/skills/ and ~/.may/subagents/ on first run.
//! Session sandboxes get symlinks pointing to these directories.

use std::fs;
use std::path::Path;

use async_fs;

use include_dir::{Dir, include_dir};

// Embed the skills and subagents directories at compile time
static SKILLS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/skills");
static SUBAGENTS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/bundle/subagents");

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
    /// Creates ~/.may/skills/ and ~/.may/subagents/ directories.
    pub fn ensure_extracted(may_dir: &Path) -> Result<(), BundleError> {
        let skills_dir = may_dir.join("skills");
        let subagents_dir = may_dir.join("subagents");

        fs::create_dir_all(&skills_dir)?;
        fs::create_dir_all(&subagents_dir)?;

        // Extract embedded skills
        Self::extract_dir(&SKILLS_DIR, &skills_dir)?;

        // Extract embedded subagents
        Self::extract_dir(&SUBAGENTS_DIR, &subagents_dir)?;

        Ok(())
    }

    /// Create symlinks in session sandbox pointing to global resources.
    pub async fn create_sandbox_symlinks(
        sandbox_dir: &Path,
        may_dir: &Path,
    ) -> Result<(), BundleError> {
        let subagents_target = may_dir.join("subagents");
        let skills_target = may_dir.join("skills");
        let memory_target = may_dir.join("memory.md");

        Self::create_dir_symlink(sandbox_dir, &subagents_target, ".subagents").await?;
        Self::create_dir_symlink(sandbox_dir, &skills_target, ".skills").await?;
        Self::create_file_symlink(sandbox_dir, &memory_target, "memory.md").await?;

        // Also create artifacts directory in sandbox
        async_fs::create_dir_all(sandbox_dir.join("artifacts")).await?;

        Ok(())
    }

    /// Recursively extract an embedded directory to the filesystem.
    fn extract_dir(src: &Dir<'_>, dest: &Path) -> Result<(), BundleError> {
        for file in src.files() {
            let file_path = dest.join(file.path());
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            // Only write if file doesn't exist (preserve user modifications)
            if !file_path.exists() {
                fs::write(&file_path, file.contents())?;
            }
        }

        for dir in src.dirs() {
            Self::extract_dir(dir, dest)?;
        }

        Ok(())
    }

    /// Create file symlink from parent/name to target file.
    async fn create_file_symlink(
        parent: &Path,
        target: &Path,
        name: &str,
    ) -> Result<(), BundleError> {
        let link = parent.join(name);

        if let Ok(metadata) = async_fs::symlink_metadata(&link).await {
            if metadata.file_type().is_symlink() {
                if let Ok(existing_target) = async_fs::read_link(&link).await {
                    if existing_target == target {
                        return Ok(());
                    }
                }
                let _ = async_fs::remove_file(&link).await;
            }
        }

        #[cfg(unix)]
        async_fs::unix::symlink(target, &link).await?;

        #[cfg(windows)]
        async_fs::windows::symlink_file(target, &link).await?;

        Ok(())
    }

    /// Create directory symlink from parent/name to target.
    async fn create_dir_symlink(
        parent: &Path,
        target: &Path,
        name: &str,
    ) -> Result<(), BundleError> {
        let link = parent.join(name);

        // Skip if already correct
        if let Ok(metadata) = async_fs::symlink_metadata(&link).await {
            if metadata.file_type().is_symlink() {
                if let Ok(existing_target) = async_fs::read_link(&link).await {
                    if existing_target == target {
                        return Ok(());
                    }
                }
                let _ = async_fs::remove_file(&link).await;
            }
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
