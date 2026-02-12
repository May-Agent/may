//! Skill management for May.
//!
//! Skills are Markdown-based capability definitions stored in `~/.may/skills/`.
//! Each skill is a directory containing a `SKILL.md` file with YAML frontmatter
//! and optional subagents, references, and assets.
//!
//! Skills can be bundled with the application or installed from GitHub repositories.

use std::path::{Path, PathBuf};

use async_fs;
use blocking::unblock;
use futures_lite::StreamExt;
use serde::{Deserialize, Serialize};

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

/// Parse YAML frontmatter (`---` delimited) from content.
fn parse_frontmatter(content: &str) -> Option<SkillFrontmatter> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
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
    pub async fn list_skills(&self) -> Result<Vec<SkillInfo>, SkillError> {
        let mut skills = Vec::new();

        if async_fs::metadata(&self.skills_dir).await.is_err() {
            return Ok(skills);
        }

        let mut entries = async_fs::read_dir(&self.skills_dir).await?;
        while let Some(entry) = entries.try_next().await? {
            let path = entry.path();
            if async_fs::metadata(&path).await?.is_dir() {
                let source = self.detect_source(&path).await;
                if let Some(skill) = self.parse_skill(&path, source).await {
                    skills.push(skill);
                }
            }
        }

        // Sort by name
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(skills)
    }

    /// Parse a single skill from its directory.
    async fn parse_skill(&self, skill_dir: &Path, source: SkillSource) -> Option<SkillInfo> {
        let skill_md = skill_dir.join("SKILL.md");
        if async_fs::metadata(&skill_md).await.is_err() {
            return None;
        }

        let content = async_fs::read_to_string(&skill_md).await.ok()?;

        // Parse YAML frontmatter (between --- markers)
        let metadata = parse_frontmatter(&content)?;

        Some(SkillInfo {
            name: metadata.name,
            description: metadata.description,
            dir_name: skill_dir.file_name()?.to_string_lossy().to_string(),
            path: skill_dir.to_path_buf(),
            source,
        })
    }



    /// Detect skill source from metadata file.
    async fn detect_source(&self, skill_dir: &Path) -> SkillSource {
        let metadata_path = skill_dir.join(".source.json");
        if async_fs::metadata(&metadata_path).await.is_ok() {
            if let Ok(content) = async_fs::read_to_string(&metadata_path).await {
                if let Ok(source) = serde_json::from_str(&content) {
                    return source;
                }
            }
        }
        SkillSource::Bundled
    }

    /// Install skills from a GitHub repository.
    ///
    /// The repo should have a `skills/` directory containing skill folders.
    /// Uses git2 for cloning (non-blocking via unblock).
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
        let repo_url = format!("https://github.com/{}/{}.git", owner, repo);
        let skills_dir = self.skills_dir.clone();
        let owner = owner.to_string();
        let repo_name = repo.to_string();

        // Run blocking git operations in background thread
        let installed = unblock(move || -> Result<Vec<SkillInfo>, SkillError> {
            let temp_dir = tempfile::tempdir()?;
            let temp_path = temp_dir.path();

            // Configure fetch options for shallow clone
            let mut fetch_opts = git2::FetchOptions::new();
            fetch_opts.depth(1);

            // Clone the repository (shallow)
            let mut builder = git2::build::RepoBuilder::new();
            builder.fetch_options(fetch_opts);

            builder
                .clone(&repo_url, temp_path)
                .map_err(|_| SkillError::GitCloneFailed(repo_url.clone()))?;

            // Check for skills directory
            let skills_source = temp_path.join("skills");
            if !skills_source.exists() {
                return Err(SkillError::NoSkillsDirectory);
            }

            // Ensure target skills directory exists
            std::fs::create_dir_all(&skills_dir)?;

            let source = SkillSource::GitHub {
                owner: owner.clone(),
                repo: repo_name.clone(),
            };

            let mut installed = Vec::new();
            for entry in std::fs::read_dir(&skills_source)? {
                let entry = entry?;
                let src = entry.path();
                if src.is_dir() {
                    let skill_name = src.file_name().unwrap().to_string_lossy().to_string();
                    let dest = skills_dir.join(&skill_name);

                    // Copy skill directory recursively
                    copy_dir_recursive(&src, &dest)?;

                    // Write source metadata
                    let metadata_path = dest.join(".source.json");
                    let content = serde_json::to_string_pretty(&source)?;
                    std::fs::write(metadata_path, content)?;

                    // Parse skill info
                    if let Some(skill) = parse_skill_sync(&dest, source.clone()) {
                        installed.push(skill);
                    }
                }
            }

            Ok(installed)
        })
        .await?;

        Ok(installed)
    }

    /// Delete a skill by directory name.
    pub async fn delete_skill(&self, dir_name: &str) -> Result<(), SkillError> {
        let skill_dir = self.skills_dir.join(dir_name);
        if async_fs::metadata(&skill_dir).await.is_err() {
            return Err(SkillError::NotFound(dir_name.to_string()));
        }

        async_fs::remove_dir_all(&skill_dir).await?;
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

/// Copy a directory recursively (standalone for use in closures).
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<(), SkillError> {
    std::fs::create_dir_all(dest)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let entry_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        if entry_path.is_dir() {
            copy_dir_recursive(&entry_path, &dest_path)?;
        } else {
            std::fs::copy(&entry_path, &dest_path)?;
        }
    }

    Ok(())
}

/// Parse a skill from its directory (standalone for use in closures).
fn parse_skill_sync(skill_dir: &Path, source: SkillSource) -> Option<SkillInfo> {
    let skill_md = skill_dir.join("SKILL.md");
    let content = std::fs::read_to_string(&skill_md).ok()?;
    let metadata = parse_frontmatter(&content)?;
    Some(SkillInfo {
        name: metadata.name,
        description: metadata.description,
        dir_name: skill_dir.file_name()?.to_string_lossy().to_string(),
        path: skill_dir.to_path_buf(),
        source,
    })
}
