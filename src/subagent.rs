//! Subagent registration and discovery.
//!
//! Subagents are Markdown files with versioned YAML frontmatter, stored in
//! `~/.may/subagents/`. Registration is explicit and validated before copy.

use std::path::{Path, PathBuf};

use crate::session::{MayHomePaths, ProfileHomePaths};
use aither_agent::SubagentDefinition;

/// Subagent management error.
#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid subagent path: {0}")]
    InvalidPath(String),
    #[error("Invalid subagent definition: {0}")]
    InvalidDefinition(String),
    #[error("Subagent already exists: {0}")]
    AlreadyExists(String),
}

/// Lightweight subagent metadata.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SubagentInfo {
    pub id: String,
    pub description: String,
    pub file_name: String,
    pub path: PathBuf,
}

/// Subagent manager.
#[derive(Debug)]
pub struct SubagentManager {
    subagents_dir: PathBuf,
}

impl SubagentManager {
    /// Create a new manager rooted at `~/.may/subagents`.
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        Self::from_subagents_dir(MayHomePaths::new(may_dir.to_path_buf()).subagents_dir())
    }

    /// Create a manager rooted at one profile home.
    #[must_use]
    pub fn from_profile_home(profile_home: &ProfileHomePaths) -> Self {
        Self::from_subagents_dir(profile_home.subagents_dir())
    }

    /// Create a manager rooted at an explicit subagents directory.
    #[must_use]
    pub const fn from_subagents_dir(subagents_dir: PathBuf) -> Self {
        Self { subagents_dir }
    }

    /// Get the subagents directory path.
    #[must_use]
    pub fn subagents_dir(&self) -> &Path {
        &self.subagents_dir
    }

    /// Register one or more subagent definitions from a local path.
    ///
    /// - If `source` is a file, it must be a `.md` subagent definition.
    /// - If `source` is a directory, all top-level `.md` files are registered.
    pub async fn register_from_local_path(
        &self,
        source: &Path,
    ) -> Result<Vec<SubagentInfo>, SubagentError> {
        let meta = tokio::fs::metadata(source).await?;
        tokio::fs::create_dir_all(&self.subagents_dir).await?;

        if meta.is_file() {
            return self
                .register_single_file(source)
                .await
                .map(|item| vec![item]);
        }
        if !meta.is_dir() {
            return Err(SubagentError::InvalidPath(format!(
                "{} is neither file nor directory",
                source.display()
            )));
        }

        let mut entries = tokio::fs::read_dir(source).await?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                files.push(path);
            }
        }
        if files.is_empty() {
            return Err(SubagentError::InvalidPath(format!(
                "no .md subagent files found in {}",
                source.display()
            )));
        }

        let mut planned = Vec::with_capacity(files.len());
        for file in &files {
            let def = SubagentDefinition::from_file_async(file).await?;
            let file_name = format!("{}.md", def.id);
            let dest = self.subagents_dir.join(&file_name);
            if tokio::fs::try_exists(&dest).await? {
                return Err(SubagentError::AlreadyExists(file_name));
            }
            planned.push((file.clone(), def, dest));
        }

        let mut registered = Vec::with_capacity(planned.len());
        for (src, def, dest) in planned {
            tokio::fs::copy(&src, &dest).await?;
            registered.push(SubagentInfo {
                id: def.id,
                description: def.description,
                file_name: dest
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        SubagentError::InvalidPath(format!(
                            "invalid destination filename: {}",
                            dest.display()
                        ))
                    })?
                    .to_string(),
                path: dest,
            });
        }

        Ok(registered)
    }

    async fn register_single_file(&self, source: &Path) -> Result<SubagentInfo, SubagentError> {
        if source.extension().and_then(|ext| ext.to_str()) != Some("md") {
            return Err(SubagentError::InvalidPath(format!(
                "subagent file must end with .md: {}",
                source.display()
            )));
        }

        let def = SubagentDefinition::from_file_async(source).await?;
        let file_name = format!("{}.md", def.id);
        let dest = self.subagents_dir.join(&file_name);

        if tokio::fs::try_exists(&dest).await? {
            return Err(SubagentError::AlreadyExists(file_name));
        }

        tokio::fs::copy(source, &dest).await?;

        Ok(SubagentInfo {
            id: def.id,
            description: def.description,
            file_name: dest
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    SubagentError::InvalidPath(format!(
                        "invalid destination filename: {}",
                        dest.display()
                    ))
                })?
                .to_string(),
            path: dest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_single_subagent_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let may_dir = temp.path().join(".may");
        let source = temp.path().join("worker.md");
        tokio::fs::write(
            &source,
            "---\nversion: 1\nname: worker\ndescription: test worker\n---\n\nYou are a worker.",
        )
        .await
        .expect("write source");

        let manager = SubagentManager::new(&may_dir);
        let result = manager
            .register_from_local_path(&source)
            .await
            .expect("register subagent");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "worker");
        assert!(manager.subagents_dir().join("worker.md").exists());
    }

    #[test]
    fn bundled_subagent_files_use_current_definition_version() {
        for path in [
            "src/bundle/subagents/explore.md",
            "src/bundle/subagents/plan.md",
            "src/bundle/subagents/research.md",
            "skills/slide/subagents/art_direction.md",
            "skills/slide/subagents/slide_creator.md",
        ] {
            let content = std::fs::read_to_string(path).expect("read subagent file");
            let definition = SubagentDefinition::parse(&content)
                .unwrap_or_else(|error| panic!("failed to parse {path}: {error}"));
            assert!(
                !definition.id.trim().is_empty(),
                "subagent id must not be empty for {path}"
            );
        }
    }
}
