//! Bundled resources extraction.
//!
//! Skills are embedded in the binary and extracted
//! to the canonical May home on first run, beside aither's bundled subagent
//! definitions.
//! Local/heel session sandboxes get symlinks pointing to their home-scoped resources.
//! Container sessions use mount points managed by container lifecycle code.

use std::path::{Path, PathBuf};

use async_fs;

use include_dir::{Dir, include_dir};

use crate::session::SessionPaths;

// Embed the skills directory at compile time
static SKILLS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/skills");

// The subagent definitions may ships: aither 0.24 left bundled subagents to
// the embedder.
static SUBAGENTS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/subagents");

/// The names of the bundled subagent types, in catalog order.
pub const BUNDLED_SUBAGENT_NAMES: [&str; 6] =
    ["explore", "research", "audit", "review", "plan", "general"];

/// The bundled subagent definitions, parsed once per call site.
///
/// # Panics
///
/// Panics when a bundled definition is invalid.
#[must_use]
pub fn bundled_subagents() -> Vec<aither_agent::SubagentDefinition> {
    BUNDLED_SUBAGENT_NAMES
        .iter()
        .map(|name| {
            let file = SUBAGENTS_DIR
                .get_file(format!("{name}.toml"))
                .unwrap_or_else(|| panic!("bundled subagent {name} is embedded"));
            let source = file.contents_utf8().expect("subagent TOML is UTF-8");
            aither_agent::SubagentDefinition::parse(source)
                .unwrap_or_else(|error| panic!("bundled subagent '{name}': {error}"))
        })
        .collect()
}

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
    /// Creates ~/.may/skills/ directory.
    ///
    /// # Errors
    ///
    /// Returns [`BundleError`] when a bundled resource directory or file
    /// cannot be extracted under `may_dir`.
    pub async fn ensure_extracted(may_dir: &Path) -> Result<(), BundleError> {
        let skills_dir = may_dir.join("skills");
        Self::ensure_materialized_dir(&skills_dir).await?;

        // Built-in resources must stay in lockstep with the running binary.
        // Always overwrite bundled content to avoid stale skills after upgrades.
        Self::extract_dir_overwrite(&SKILLS_DIR, &skills_dir).await?;
        Self::extract_dir_overwrite(&SUBAGENTS_DIR, &Self::subagents_dir(may_dir)).await?;

        Ok(())
    }

    /// The global subagents directory: the definitions `subagent <name>` runs.
    #[must_use]
    pub fn subagents_dir(may_dir: &Path) -> PathBuf {
        may_dir.join("subagents")
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
        let skills_target = home_root.join("skills");

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

    /// Create directory symlink from parent/name to target.
    pub(crate) async fn create_dir_symlink(
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

#[cfg(test)]
mod tests {
    use include_dir::{Dir, DirEntry};

    use super::SKILLS_DIR;

    fn collect_dirs<'a>(dir: &'a Dir<'a>, found: &mut Vec<&'a std::path::Path>) {
        for entry in dir.entries() {
            if let DirEntry::Dir(sub) = entry {
                found.push(sub.path());
                collect_dirs(sub, found);
            }
        }
    }

    #[test]
    fn new_repo_skill_links_every_reference_file() {
        let skill = SKILLS_DIR
            .get_file("new-repo/SKILL.md")
            .and_then(|file| file.contents_utf8())
            .expect("new-repo/SKILL.md is bundled");
        assert!(skill.starts_with("---\nname: new-repo\n"));
        let references = SKILLS_DIR
            .get_dir("new-repo/references")
            .expect("new-repo/references is bundled");
        let mut count = 0;
        for file in references.files() {
            let name = file.path().file_name().unwrap().to_string_lossy();
            assert!(
                skill.contains(&format!("references/{name}")),
                "SKILL.md does not link {name}"
            );
            count += 1;
        }
        assert_eq!(count, 4);
    }

    #[test]
    fn bundled_skills_have_no_subagents_directory() {
        let mut dirs = Vec::new();
        collect_dirs(&SKILLS_DIR, &mut dirs);
        let offending: Vec<_> = dirs
            .iter()
            .filter(|path| path.file_name().is_some_and(|name| name == "subagents"))
            .collect();
        assert!(offending.is_empty(), "skill-local subagents: {offending:?}");
    }
}
