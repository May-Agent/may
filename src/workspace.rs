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

/// Error type for workspace configuration operations.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceConfigError {
    /// Failed to read workspace config.
    #[error("Failed to read workspace config: {0}")]
    Read(#[from] std::io::Error),
    /// Failed to parse workspace config.
    #[error("Failed to parse workspace config: {0}")]
    Parse(#[from] toml::de::Error),
    /// Failed to serialize workspace config.
    #[error("Failed to serialize workspace config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

impl WorkspaceConfig {
    /// Load workspace config from file.
    ///
    /// Returns `Ok(default)` if the file does not exist,
    /// but propagates parse errors so corrupt configs are not silently dropped.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceConfigError::Read`] when the file exists but cannot
    /// be read, and [`WorkspaceConfigError`] when its TOML fails to parse.
    pub fn load(config_path: &Path) -> Result<Self, WorkspaceConfigError> {
        match fs::read_to_string(config_path) {
            Ok(content) => Ok(toml::from_str(&content)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(WorkspaceConfigError::Read(e)),
        }
    }

    /// Save workspace config to file.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceConfigError`] when serializing or writing the file
    /// fails.
    pub fn save(&self, config_path: &Path) -> Result<(), WorkspaceConfigError> {
        let content = toml::to_string_pretty(self)?;
        fs::write(config_path, content)?;
        Ok(())
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
    #[must_use]
    pub fn has_write_access(&self, path: &Path) -> bool {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.workdirs
            .iter()
            .any(|d| canonical.starts_with(&d.path) && d.permission == Permission::ReadWrite)
    }
}

/// The complete workspace state for a May session.
#[derive(Debug)]
pub struct Workspace {
    /// Session sandbox directory (always read/write).
    pub sandbox: PathBuf,
    /// Current working directory (read-only by default).
    pub cwd: PathBuf,
    /// User-configured workspace directories.
    pub config: WorkspaceConfig,
    /// Profile-scoped workspace configuration path.
    config_path: PathBuf,
}

impl Workspace {
    /// Create a new workspace.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceConfigError`] when loading the workspace config
    /// fails.
    pub fn new(
        sandbox: PathBuf,
        cwd: PathBuf,
        config_path: PathBuf,
    ) -> Result<Self, WorkspaceConfigError> {
        let config = WorkspaceConfig::load(&config_path)?;
        Ok(Self {
            sandbox,
            cwd,
            config,
            config_path,
        })
    }

    /// Add a working directory (requires user approval).
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceConfigError`] when saving the updated config fails.
    pub fn add_workdir(
        &mut self,
        path: PathBuf,
        description: Option<String>,
    ) -> Result<(), WorkspaceConfigError> {
        self.config.add_workdir(path, description);
        self.config.save(&self.config_path)
    }

    /// Get all readable directories.
    #[must_use]
    pub fn readable_dirs(&self) -> Vec<&PathBuf> {
        let mut dirs = vec![&self.sandbox, &self.cwd];
        for wd in &self.config.workdirs {
            dirs.push(&wd.path);
        }
        dirs
    }

    /// Get all writable directories.
    #[must_use]
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
