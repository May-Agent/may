//! Skill management for May.
//!
//! Skills are Markdown-based capability definitions stored in `~/.may/skills/`.
//! Each skill is a directory containing a `SKILL.md` file with YAML frontmatter
//! and optional subagents, references, and assets.
//!
//! Skills can be bundled with the application or installed from GitHub repositories.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Skill management error.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yml::Error),
    #[error("Git clone failed for {0}")]
    GitCloneFailed(String),
    #[error("Repository has no skills/ directory")]
    NoSkillsDirectory,
    #[error("Invalid repo format, expected owner/repo")]
    InvalidRepoFormat,
    #[error("Skill not found: {0}")]
    NotFound(String),
}

/// Origin of a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum SkillSource {
    /// Bundled with the application.
    Bundled,
    /// Installed from GitHub.
    GitHub { owner: String, repo: String },
}

impl std::fmt::Display for SkillSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillSource::Bundled => write!(f, "bundled"),
            SkillSource::GitHub { owner, repo } => write!(f, "github:{}/{}", owner, repo),
        }
    }
}

/// Skill metadata parsed from SKILL.md frontmatter.
#[derive(Debug, Clone)]
pub struct SkillInfo {
    /// Skill name from frontmatter.
    pub name: String,
    /// Skill description from frontmatter.
    pub description: String,
    /// Directory name (skill folder name).
    pub dir_name: String,
    /// Full path to skill directory.
    pub path: PathBuf,
    /// Source of the skill.
    pub source: SkillSource,
}

/// SKILL.md frontmatter structure.
#[derive(Debug, Clone, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

/// Skill manager for listing, installing, and managing skills.
#[derive(Debug)]
pub struct SkillManager {
    /// Skills directory path (~/.may/skills).
    skills_dir: PathBuf,
}

impl SkillManager {
    /// Create a new skill manager.
    pub fn new(may_dir: &Path) -> Self {
        Self {
            skills_dir: may_dir.join("skills"),
        }
    }

    /// Get the skills directory path.
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// List all installed skills.
    pub fn list_skills(&self) -> Result<Vec<SkillInfo>, SkillError> {
        let mut skills = Vec::new();

        if !self.skills_dir.exists() {
            return Ok(skills);
        }

        for entry in std::fs::read_dir(&self.skills_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let source = self.detect_source(&path);
                if let Some(skill) = self.parse_skill(&path, source) {
                    skills.push(skill);
                }
            }
        }

        // Sort by name
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// Parse a single skill from its directory.
    fn parse_skill(&self, skill_dir: &Path, source: SkillSource) -> Option<SkillInfo> {
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&skill_md).ok()?;

        // Parse YAML frontmatter (between --- markers)
        let metadata = self.parse_frontmatter(&content)?;

        Some(SkillInfo {
            name: metadata.name,
            description: metadata.description,
            dir_name: skill_dir.file_name()?.to_string_lossy().to_string(),
            path: skill_dir.to_path_buf(),
            source,
        })
    }

    /// Parse YAML frontmatter from content.
    fn parse_frontmatter(&self, content: &str) -> Option<SkillFrontmatter> {
        let mut lines = content.lines();
        let first_line = lines.next()?;
        if first_line.trim() != "---" {
            return None;
        }

        let mut frontmatter = String::new();
        for line in lines {
            if line.trim() == "---" {
                break;
            }
            frontmatter.push_str(line);
            frontmatter.push('\n');
        }

        serde_yml::from_str(&frontmatter).ok()
    }

    /// Detect skill source from metadata file.
    fn detect_source(&self, skill_dir: &Path) -> SkillSource {
        let metadata_path = skill_dir.join(".source.json");
        if metadata_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&metadata_path) {
                if let Ok(source) = serde_json::from_str(&content) {
                    return source;
                }
            }
        }
        SkillSource::Bundled
    }

    /// Write source metadata to skill directory.
    fn write_source_metadata(&self, skill_dir: &Path, source: &SkillSource) -> Result<(), SkillError> {
        let metadata_path = skill_dir.join(".source.json");
        let content = serde_json::to_string_pretty(source)?;
        std::fs::write(metadata_path, content)?;
        Ok(())
    }

    /// Install skills from a GitHub repository.
    ///
    /// The repo should have a `skills/` directory containing skill folders.
    /// Uses sparse checkout to only download the skills directory.
    ///
    /// # Arguments
    /// * `owner` - GitHub repository owner
    /// * `repo` - GitHub repository name
    ///
    /// # Returns
    /// List of installed skills
    pub async fn install_from_github(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<SkillInfo>, SkillError> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path = temp_dir.path();

        let repo_url = format!("https://github.com/{}/{}.git", owner, repo);

        // Clone with sparse checkout (only download skills directory)
        let status = Command::new("git")
            .args([
                "clone",
                "--filter=blob:none",
                "--no-checkout",
                "--depth=1",
                &repo_url,
                ".",
            ])
            .current_dir(temp_path)
            .status()
            .await?;

        if !status.success() {
            return Err(SkillError::GitCloneFailed(repo_url));
        }

        // Configure sparse checkout
        let _ = Command::new("git")
            .args(["sparse-checkout", "init", "--cone"])
            .current_dir(temp_path)
            .status()
            .await?;

        let _ = Command::new("git")
            .args(["sparse-checkout", "set", "skills"])
            .current_dir(temp_path)
            .status()
            .await?;

        // Checkout
        let _ = Command::new("git")
            .args(["checkout"])
            .current_dir(temp_path)
            .status()
            .await?;

        // Copy skills to ~/.may/skills/
        let skills_source = temp_path.join("skills");
        if !skills_source.exists() {
            return Err(SkillError::NoSkillsDirectory);
        }

        // Ensure skills directory exists
        std::fs::create_dir_all(&self.skills_dir)?;

        let source = SkillSource::GitHub {
            owner: owner.to_string(),
            repo: repo.to_string(),
        };

        let mut installed = Vec::new();
        for entry in std::fs::read_dir(&skills_source)? {
            let entry = entry?;
            let src = entry.path();
            if src.is_dir() {
                let skill_name = src.file_name().unwrap().to_string_lossy().to_string();
                let dest = self.skills_dir.join(&skill_name);

                // Copy skill directory recursively
                self.copy_dir_recursive(&src, &dest)?;

                // Write source metadata
                self.write_source_metadata(&dest, &source)?;

                if let Some(skill) = self.parse_skill(&dest, source.clone()) {
                    installed.push(skill);
                }
            }
        }

        Ok(installed)
    }

    /// Copy a directory recursively.
    fn copy_dir_recursive(&self, src: &Path, dest: &Path) -> Result<(), SkillError> {
        std::fs::create_dir_all(dest)?;

        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let entry_path = entry.path();
            let dest_path = dest.join(entry.file_name());

            if entry_path.is_dir() {
                self.copy_dir_recursive(&entry_path, &dest_path)?;
            } else {
                std::fs::copy(&entry_path, &dest_path)?;
            }
        }

        Ok(())
    }

    /// Delete a skill by directory name.
    pub fn delete_skill(&self, dir_name: &str) -> Result<(), SkillError> {
        let skill_dir = self.skills_dir.join(dir_name);
        if !skill_dir.exists() {
            return Err(SkillError::NotFound(dir_name.to_string()));
        }

        std::fs::remove_dir_all(&skill_dir)?;
        Ok(())
    }

    /// Parse a GitHub repo string in "owner/repo" format.
    pub fn parse_github_repo(input: &str) -> Result<(&str, &str), SkillError> {
        let input = input.trim();
        match input.split_once('/') {
            Some((owner, repo)) if !owner.is_empty() && !repo.is_empty() => Ok((owner, repo)),
            _ => Err(SkillError::InvalidRepoFormat),
        }
    }
}
