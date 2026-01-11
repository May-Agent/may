//! Bundled resources extraction.
//!
//! Skills and subagents are embedded in the binary and extracted
//! to ~/.may/skills/ and ~/.may/subagents/ on first run.
//! Session sandboxes get symlinks pointing to these directories.

use std::fs;
use std::path::Path;

use include_dir::{include_dir, Dir};

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
    pub fn create_sandbox_symlinks(sandbox_dir: &Path, may_dir: &Path) -> Result<(), BundleError> {
        let subagents_target = may_dir.join("subagents");
        let skills_target = may_dir.join("skills");
        let memory_target = may_dir.join("memory.md");

        Self::create_dir_symlink(sandbox_dir, &subagents_target, ".subagents")?;
        Self::create_dir_symlink(sandbox_dir, &skills_target, ".skills")?;
        Self::create_file_symlink(sandbox_dir, &memory_target, "memory.md")?;

        // Also create artifacts directory in sandbox
        fs::create_dir_all(sandbox_dir.join("artifacts"))?;

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
    fn create_file_symlink(parent: &Path, target: &Path, name: &str) -> Result<(), BundleError> {
        let link = parent.join(name);

        if link.is_symlink() {
            if let Ok(existing_target) = fs::read_link(&link) {
                if existing_target == target {
                    return Ok(());
                }
            }
            fs::remove_file(&link).ok();
        }

        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &link)?;

        #[cfg(windows)]
        std::os::windows::fs::symlink_file(target, &link)?;

        Ok(())
    }

    /// Create directory symlink from parent/name to target.
    fn create_dir_symlink(parent: &Path, target: &Path, name: &str) -> Result<(), BundleError> {
        let link = parent.join(name);

        // Skip if already correct
        if link.is_symlink() {
            if let Ok(existing_target) = fs::read_link(&link) {
                if existing_target == target {
                    return Ok(());
                }
            }
            fs::remove_file(&link).ok();
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, &link)?;
        }

        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, &link)?;
        }

        Ok(())
    }
}
