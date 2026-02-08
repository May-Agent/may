//! Workspace management for directory permissions.
//!
//! May operates with different directory permission levels:
//! - **Sandbox**: Full read/write access (session-specific storage)
//! - **Working directories**: User-approved directories for read/write access
//! - **Current directory**: Read-only access by default

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Permission level for a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    /// Read-only access.
    ReadOnly,
    /// Full read/write access.
    ReadWrite,
}

/// A workspace directory with its permission level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceDir {
    /// Absolute path to the directory.
    pub path: PathBuf,
    /// Permission level.
    pub permission: Permission,
    /// Human-readable description (optional).
    #[serde(default)]
    pub description: Option<String>,
}

/// Workspace configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Working directories with read/write access (user-approved).
    #[serde(default)]
    pub workdirs: Vec<WorkspaceDir>,
}

impl WorkspaceConfig {
    /// Load workspace config from file.
    pub fn load(may_dir: &Path) -> Self {
        let config_path = may_dir.join("workspace.toml");
        match fs::read_to_string(&config_path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Save workspace config to file.
    pub fn save(&self, may_dir: &Path) -> std::io::Result<()> {
        let config_path = may_dir.join("workspace.toml");
        let content = toml::to_string_pretty(self).unwrap_or_default();
        fs::write(config_path, content)
    }

    /// Add a working directory with read/write access.
    pub fn add_workdir(&mut self, path: PathBuf, description: Option<String>) {
        // Canonicalize the path
        let canonical = path.canonicalize().unwrap_or(path);

        // Check if already exists
        if self.workdirs.iter().any(|d| d.path == canonical) {
            return;
        }

        self.workdirs.push(WorkspaceDir {
            path: canonical,
            permission: Permission::ReadWrite,
            description,
        });
    }

    /// Remove a working directory.
    pub fn remove_workdir(&mut self, path: &Path) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.workdirs.retain(|d| d.path != canonical);
    }

    /// Check if a path has read/write access.
    pub fn has_write_access(&self, path: &Path) -> bool {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.workdirs
            .iter()
            .any(|d| canonical.starts_with(&d.path) && d.permission == Permission::ReadWrite)
    }
}

/// The complete workspace state for a May session.
pub struct Workspace {
    /// Session sandbox directory (always read/write).
    pub sandbox: PathBuf,
    /// Current working directory (read-only by default).
    pub cwd: PathBuf,
    /// User-configured workspace directories.
    pub config: WorkspaceConfig,
    /// May config directory.
    may_dir: PathBuf,
}

impl Workspace {
    /// Create a new workspace.
    pub fn new(sandbox: PathBuf, cwd: PathBuf, may_dir: PathBuf) -> Self {
        let config = WorkspaceConfig::load(&may_dir);
        Self {
            sandbox,
            cwd,
            config,
            may_dir,
        }
    }

    /// Add a working directory (requires user approval).
    pub fn add_workdir(&mut self, path: PathBuf, description: Option<String>) {
        self.config.add_workdir(path, description);
        let _ = self.config.save(&self.may_dir);
    }

    /// Get all readable directories.
    pub fn readable_dirs(&self) -> Vec<&PathBuf> {
        let mut dirs = vec![&self.sandbox, &self.cwd];
        for wd in &self.config.workdirs {
            dirs.push(&wd.path);
        }
        dirs
    }

    /// Get all writable directories.
    pub fn writable_dirs(&self) -> Vec<&PathBuf> {
        let mut dirs = vec![&self.sandbox];
        for wd in &self.config.workdirs {
            if wd.permission == Permission::ReadWrite {
                dirs.push(&wd.path);
            }
        }
        dirs
    }
}
