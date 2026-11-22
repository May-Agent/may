//! Skill management for May.
//!
//! Skills are Markdown-based capability definitions stored in `~/.may/skills/`.
//! Each skill is a directory containing a `SKILL.md` file with YAML frontmatter
//! and optional references and assets.
//!
//! Skills can be bundled with the application or installed from GitHub repositories.

use std::path::{Path, PathBuf};

use async_fs;
use blocking::unblock;
use futures_lite::StreamExt;
use serde::{Deserialize, Serialize};

use crate::session::{MayHomePaths, ProfileHomePaths};
use may_config::fs_utils::copy_dir_contents_recursive;

/// Skill management error.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// YAML error.
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yml::Error),
    /// Cloning the skill repository failed.
    #[error("Git clone failed for {0}")]
    GitCloneFailed(String),
    /// Repository has no skills/ directory.
    #[error("Repository has no skills/ directory")]
    NoSkillsDirectory,
    /// Invalid repository spec, expected `owner/repo`, a `github:`/`gitlab:`
    /// binding or a forge clone URL.
    #[error("invalid repository spec '{0}': {1}")]
    InvalidRepoSpec(String, String),
    /// The May configuration could not be read.
    #[error(transparent)]
    Config(#[from] may_config::config::ConfigError),
    /// Skill not found.
    #[error("Skill not found: {0}")]
    NotFound(String),
    /// Invalid skill path.
    #[error("Invalid skill path: {0}")]
    InvalidPath(String),
    /// Invalid skill definition.
    #[error("Invalid skill definition: {0}")]
    InvalidDefinition(String),
    /// Skill already exists.
    #[error("Skill already exists: {0}")]
    AlreadyExists(String),
}

/// Origin of a skill.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, skyzen::ToSchema)]
#[serde(tag = "type")]
pub enum SkillSource {
    /// Bundled with the application.
    Bundled,
    /// Installed from GitHub.
    GitHub {
        /// Repository owner.
        owner: String,
        /// Repository name.
        repo: String,
        /// The GitHub Enterprise host; absent for `github.com`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
    },
    /// Installed from GitLab (`gitlab.com` or a self-hosted host).
    GitLab {
        /// The forge host.
        host: String,
        /// The `group/sub/.../repo` namespace path.
        path: String,
    },
    /// Registered from a local project path.
    LocalPath {
        /// Absolute path of the directory the skill was registered from.
        path: String,
    },
}

impl std::fmt::Display for SkillSource {
    /// The canonical binding form: `github:owner/repo`,
    /// `github:<host>/owner/repo`, `gitlab:<host>/<path>` or `local:<path>`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bundled => write!(f, "bundled"),
            Self::GitHub { owner, repo, host } => match host {
                Some(host) => write!(f, "github:{host}/{owner}/{repo}"),
                None => write!(f, "github:{owner}/{repo}"),
            },
            Self::GitLab { host, path } => write!(f, "gitlab:{host}/{path}"),
            Self::LocalPath { path } => write!(f, "local:{path}"),
        }
    }
}

/// The [`SkillSource`] recording the forge repository a skill was installed
/// from.
fn skill_source_for(repo: &may_config::forge::ForgeRepo) -> SkillSource {
    match repo.kind() {
        may_config::forge::ForgeKind::GitHub => SkillSource::GitHub {
            owner: repo.namespace().to_string(),
            repo: repo.name().to_string(),
            host: (repo.host().as_str() != may_config::forge::GITHUB_HOST)
                .then(|| repo.host().as_str().to_string()),
        },
        may_config::forge::ForgeKind::GitLab => SkillSource::GitLab {
            host: repo.host().as_str().to_string(),
            path: repo.full_path(),
        },
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
    /// The command the skill is about: the session catalog offers the skill
    /// only when that command is registered.
    pub requires_command: Option<String>,
}

/// SKILL.md frontmatter structure.
#[derive(Debug, Clone, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default, rename = "requires-command")]
    requires_command: Option<String>,
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
    #[must_use]
    pub fn new(may_dir: &Path) -> Self {
        Self::from_skills_dir(MayHomePaths::new(may_dir.to_path_buf()).skills_dir())
    }

    /// Create a manager rooted at one profile home.
    #[must_use]
    pub fn from_profile_home(profile_home: &ProfileHomePaths) -> Self {
        Self::from_skills_dir(profile_home.skills_dir())
    }

    /// Create a manager rooted at an explicit skills directory.
    #[must_use]
    pub const fn from_skills_dir(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }

    /// Get the skills directory path.
    #[must_use]
    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }

    /// List all installed skills.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError`] when the skills directory cannot be read or a
    /// skill's metadata fails to parse.
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
            requires_command: metadata.requires_command,
        })
    }

    /// Detect skill source from metadata file.
    async fn detect_source(&self, skill_dir: &Path) -> SkillSource {
        let metadata_path = skill_dir.join(".source.json");
        if async_fs::metadata(&metadata_path).await.is_ok()
            && let Ok(content) = async_fs::read_to_string(&metadata_path).await
            && let Ok(source) = serde_json::from_str(&content)
        {
            return source;
        }
        SkillSource::Bundled
    }

    /// Install skills from a forge repository (GitHub or GitLab, hosted or
    /// self-hosted).
    ///
    /// The repository's `skills/` directory is cloned over HTTPS — shallow,
    /// with the one-shot `auth` credential when given — and each skill
    /// directory it holds is copied into the skills directory with a
    /// `.source.json` recording the repository's canonical binding.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::GitCloneFailed`] when the clone fails,
    /// [`SkillError::NoSkillsDirectory`] when the repository has no `skills/`
    /// directory, or [`SkillError::Io`] when copying a skill fails.
    ///
    /// # Panics
    ///
    /// Panics if a cloned skills directory entry has no file name.
    pub async fn install_from_forge(
        &self,
        repo: &may_config::forge::ForgeRepo,
        auth: Option<may_config::host_git::HttpsAuth>,
    ) -> Result<Vec<SkillInfo>, SkillError> {
        let clone_url = repo.https_clone_url();
        self.install_from_git_url(&clone_url, repo, auth).await
    }

    /// Clone `clone_url` and install its `skills` directory, recording
    /// `repo` as the source. `install_from_forge` is this with the
    /// repository's own HTTPS URL; the split exists so tests can clone a
    /// local `file://` origin.
    ///
    /// # Errors
    /// As [`Self::install_from_forge`].
    async fn install_from_git_url(
        &self,
        clone_url: &str,
        repo: &may_config::forge::ForgeRepo,
        auth: Option<may_config::host_git::HttpsAuth>,
    ) -> Result<Vec<SkillInfo>, SkillError> {
        let clone_url = clone_url.to_string();
        let source = skill_source_for(repo);
        let skills_dir = self.skills_dir.clone();
        let mut git = may_config::host_git::HostGit::inherit();
        if let Some(auth) = auth {
            git = git.with_https_auth(auth);
        }

        // Run blocking git operations in background thread
        let installed = unblock(move || -> Result<Vec<SkillInfo>, SkillError> {
            let temp_dir = tempfile::tempdir()?;
            let temp_path = temp_dir.path();

            let mut command = git.command();
            // A skill install may never stop at a credential prompt.
            command.env("GIT_TERMINAL_PROMPT", "0");
            let output = command
                .args(["clone", "--depth", "1", &clone_url])
                .arg(temp_path)
                .output()
                .map_err(|error| SkillError::GitCloneFailed(format!("{clone_url}: {error}")))?;
            if !output.status.success() {
                return Err(SkillError::GitCloneFailed(format!(
                    "{clone_url}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }

            // Check for skills directory
            let skills_source = temp_path.join("skills");
            if !skills_source.exists() {
                return Err(SkillError::NoSkillsDirectory);
            }

            // Ensure target skills directory exists
            std::fs::create_dir_all(&skills_dir)?;

            let mut installed = Vec::new();
            for entry in std::fs::read_dir(&skills_source)? {
                let entry = entry?;
                let src = entry.path();
                if src.is_dir() {
                    let skill_name = src.file_name().unwrap().to_string_lossy().to_string();
                    let dest = skills_dir.join(&skill_name);

                    // Copy skill directory recursively
                    copy_dir_contents_recursive(&src, &dest)?;

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

    /// Install the skills the spec names, using `config`'s `[forge]`
    /// credentials when the repository's host has them.
    ///
    /// # Errors
    /// Returns [`SkillError::Config`] when the configuration cannot be read,
    /// and as [`Self::install_from_forge`] otherwise.
    pub async fn install_from_spec(&self, input: &str) -> Result<Vec<SkillInfo>, SkillError> {
        let config = may_config::config::MayConfig::load().await?;
        let repo = Self::parse_skill_repo(input, &config)?;
        let auth = match may_config::forge::ForgeAuth::new(&config)
            .git_https_auth(repo.host().as_str())
            .await
        {
            Ok(auth) => Some(auth),
            // An unauthenticated host still installs public repositories.
            Err(may_config::forge::ForgeAuthError::Unauthenticated { .. }) => None,
            Err(error) => return Err(SkillError::GitCloneFailed(error.to_string())),
        };
        self.install_from_forge(&repo, auth).await
    }

    /// Delete a skill by directory name.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::NotFound`] when no skill with `dir_name` exists, or
    /// [`SkillError::Io`] when removing its directory fails.
    pub async fn delete_skill(&self, dir_name: &str) -> Result<(), SkillError> {
        let skill_dir = self.skills_dir.join(dir_name);
        if async_fs::metadata(&skill_dir).await.is_err() {
            return Err(SkillError::NotFound(dir_name.to_string()));
        }

        async_fs::remove_dir_all(&skill_dir).await?;
        Ok(())
    }

    /// Register a skill from an existing local directory.
    ///
    /// The source directory must contain a valid `SKILL.md` frontmatter block.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError`] when the source lacks valid `SKILL.md` frontmatter
    /// or copying it into the skills directory fails.
    pub async fn register_from_local_path(&self, source: &Path) -> Result<SkillInfo, SkillError> {
        let source = source.to_path_buf();
        let skills_dir = self.skills_dir.clone();

        unblock(move || -> Result<SkillInfo, SkillError> {
            if !source.exists() || !source.is_dir() {
                return Err(SkillError::InvalidPath(format!(
                    "{} is not a directory",
                    source.display()
                )));
            }

            let source_canon = std::fs::canonicalize(&source)?;
            let skill_md = source_canon.join("SKILL.md");
            if !skill_md.exists() {
                return Err(SkillError::InvalidDefinition(format!(
                    "missing SKILL.md in {}",
                    source_canon.display()
                )));
            }

            let dir_name = source_canon
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    SkillError::InvalidPath(format!(
                        "invalid skill directory name: {}",
                        source_canon.display()
                    ))
                })?
                .to_string();

            std::fs::create_dir_all(&skills_dir)?;
            let dest = skills_dir.join(&dir_name);
            if dest.exists() {
                return Err(SkillError::AlreadyExists(dir_name));
            }

            copy_dir_contents_recursive(&source_canon, &dest)?;
            let source_meta = SkillSource::LocalPath {
                path: source_canon.display().to_string(),
            };
            let metadata_path = dest.join(".source.json");
            let metadata = serde_json::to_string_pretty(&source_meta)?;
            std::fs::write(metadata_path, metadata)?;

            parse_skill_sync(&dest, source_meta).ok_or_else(|| {
                SkillError::InvalidDefinition(format!(
                    "failed to parse SKILL.md in {}",
                    dest.display()
                ))
            })
        })
        .await
    }

    /// The forge repository a skill install spec names.
    ///
    /// Accepts `owner/repo` (GitHub.com), `github:owner/repo`,
    /// `github:<host>/owner/repo`, `gitlab:<group/.../repo>` (gitlab.com),
    /// `gitlab:<host>/<group/.../repo>`, a bare `host/path` (kind from the
    /// configured `[forge]` entry, GitLab when unset) and `https://`,
    /// `ssh://` or `git@host:path` clone URLs.
    ///
    /// # Errors
    ///
    /// Returns [`SkillError::InvalidRepoSpec`] when the spec names no forge
    /// repository.
    pub fn parse_skill_repo(
        input: &str,
        config: &may_config::config::MayConfig,
    ) -> Result<may_config::forge::ForgeRepo, SkillError> {
        let input = input.trim();
        let invalid = |error: &dyn std::fmt::Display| {
            SkillError::InvalidRepoSpec(input.to_string(), error.to_string())
        };
        if input.starts_with("github:") || input.starts_with("gitlab:") {
            return may_config::forge::ForgeRepo::parse_binding(input).map_err(|e| invalid(&e));
        }
        if input.contains("://") || input.contains('@') {
            return may_config::forge::ForgeRepo::from_remote_url(input, config)
                .map_err(|e| invalid(&e));
        }
        let binding = match input.split('/').count() {
            2 => format!("github:{input}"),
            n if n >= 3 => {
                let Some(first) = input.split('/').next() else {
                    return Err(invalid(&"empty repository spec"));
                };
                match config.forge_kind(first) {
                    Some(may_config::forge::ForgeKind::GitHub) => format!("github:{input}"),
                    _ => format!("gitlab:{input}"),
                }
            }
            _ => {
                return Err(invalid(
                    &"expected owner/repo, a github:/gitlab: binding or a clone URL",
                ));
            }
        };
        may_config::forge::ForgeRepo::parse_binding(&binding).map_err(|e| invalid(&e))
    }
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
        requires_command: metadata.requires_command,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SKILL_MD: &str = "---\nname: Test Skill\ndescription: test\n---\n\n# Test Skill";

    #[test]
    fn skill_source_serializes_and_displays_forge_bindings() {
        let cases = [
            (
                SkillSource::GitHub {
                    owner: "acme".to_string(),
                    repo: "skills".to_string(),
                    host: None,
                },
                "github:acme/skills",
            ),
            (
                SkillSource::GitHub {
                    owner: "acme".to_string(),
                    repo: "skills".to_string(),
                    host: Some("ghe.corp.example".to_string()),
                },
                "github:ghe.corp.example/acme/skills",
            ),
            (
                SkillSource::GitLab {
                    host: "gitlab.com".to_string(),
                    path: "org/team/skills".to_string(),
                },
                "gitlab:gitlab.com/org/team/skills",
            ),
            (
                SkillSource::LocalPath {
                    path: "/tmp/x".to_string(),
                },
                "local:/tmp/x",
            ),
        ];
        for (source, text) in cases {
            assert_eq!(source.to_string(), text);
            let json = serde_json::to_string(&source).expect("serialize");
            let parsed: SkillSource = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, source);
        }
        // A `.source.json` written before the forge work still reads.
        let old: SkillSource =
            serde_json::from_str(r#"{"type":"GitHub","owner":"acme","repo":"skills"}"#)
                .expect("old metadata parses");
        assert_eq!(old.to_string(), "github:acme/skills");
    }

    #[test]
    fn skill_repo_specs_parse_to_forge_repositories() {
        let config = may_config::config::MayConfig::default();
        let expect = |spec: &str, binding: &str| {
            let repo = SkillManager::parse_skill_repo(spec, &config).expect(spec);
            assert_eq!(repo.canonical_binding(), binding, "{spec}");
        };
        expect("acme/skills", "github:acme/skills");
        expect("github:acme/skills", "github:acme/skills");
        expect(
            "github:ghe.corp.example/acme/skills",
            "github:ghe.corp.example/acme/skills",
        );
        expect(
            "gitlab:org/team/skills",
            "gitlab:gitlab.com/org/team/skills",
        );
        expect(
            "gitlab:gitlab.com/org/team/skills",
            "gitlab:gitlab.com/org/team/skills",
        );
        expect(
            "gitlab:git.corp.example/ai/skills",
            "gitlab:git.corp.example/ai/skills",
        );
        expect(
            "https://gitlab.com/org/team/skills.git",
            "gitlab:gitlab.com/org/team/skills",
        );
        expect(
            "git@gitlab.com:org/team/skills.git",
            "gitlab:gitlab.com/org/team/skills",
        );
        expect(
            "git.corp.example/ai/skills",
            "gitlab:git.corp.example/ai/skills",
        );
        assert!(SkillManager::parse_skill_repo("", &config).is_err());
        assert!(SkillManager::parse_skill_repo("nonsense", &config).is_err());
    }

    #[tokio::test]
    async fn install_from_forge_clones_and_records_the_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let may_dir = temp.path().join(".may");
        let origin = temp.path().join("origin");
        std::fs::create_dir_all(origin.join("skills/demo")).expect("skills dir");
        std::fs::write(origin.join("skills/demo/SKILL.md"), SKILL_MD).expect("SKILL.md");
        let git = may_config::host_git::HostGit::inherit();
        for args in [
            vec!["init", "-b", "main"],
            vec!["add", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
        ] {
            git.output(Some(&origin), &args).expect("git setup");
        }
        let url = format!("file://{}", origin.display());
        // The clone path is exercised through a local `git clone` of the
        // origin; the binding still records the parsed forge repository.
        let repo = may_config::forge::ForgeRepo::parse_binding("gitlab:gitlab.com/org/demo")
            .expect("binding");
        let manager = SkillManager::new(&may_dir);
        let skills = manager
            .install_from_git_url(&url, &repo, None)
            .await
            .expect("install");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].source.to_string(), "gitlab:gitlab.com/org/demo");
        assert!(may_dir.join("skills/demo/.source.json").exists());
    }

    #[tokio::test]
    async fn register_skill_from_local_path_works() {
        let temp = tempfile::tempdir().expect("tempdir");
        let may_dir = temp.path().join(".may");
        let source_root = temp.path().join("project");
        let source = source_root.join("my-skill");
        tokio::fs::create_dir_all(&source)
            .await
            .expect("create source dir");
        tokio::fs::write(
            source.join("SKILL.md"),
            "---\nname: Local Skill\ndescription: local test\n---\n\n# Local Skill",
        )
        .await
        .expect("write SKILL.md");

        let manager = SkillManager::new(&may_dir);
        let info = manager
            .register_from_local_path(&source)
            .await
            .expect("register local skill");

        assert_eq!(info.dir_name, "my-skill");
        assert!(
            manager
                .skills_dir()
                .join("my-skill")
                .join("SKILL.md")
                .exists()
        );
    }
}
