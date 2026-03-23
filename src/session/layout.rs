//! Typed filesystem layout for a May session sandbox.

use std::path::{Path, PathBuf};

/// Canonical paths for a single session sandbox.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    root: PathBuf,
}

/// Canonical paths for the current May home root.
#[derive(Debug, Clone)]
pub struct MayHomePaths {
    root: PathBuf,
}

/// Canonical paths for one profile home environment rooted under the May home.
#[derive(Debug, Clone)]
pub struct ProfileHomePaths {
    root: PathBuf,
}

impl MayHomePaths {
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    #[must_use]
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    #[must_use]
    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    #[must_use]
    pub fn memory_file(&self) -> PathBuf {
        self.root.join("MEMORY.md")
    }

    #[must_use]
    pub fn workspace_config(&self) -> PathBuf {
        self.root.join("workspace.toml")
    }

    #[must_use]
    pub fn db_file(&self) -> PathBuf {
        self.root.join("history.db")
    }

    #[must_use]
    pub fn session_paths(&self, session_id: &str) -> SessionPaths {
        SessionPaths::new(self.sessions_dir().join(session_id))
    }

    #[must_use]
    pub fn profile_home(&self, profile_id: &str) -> ProfileHomePaths {
        ProfileHomePaths {
            root: self.profiles_dir().join(profile_id),
        }
    }
}

impl ProfileHomePaths {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    #[must_use]
    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    #[must_use]
    pub fn workspace_config(&self) -> PathBuf {
        self.root.join("workspace.toml")
    }

    #[must_use]
    pub fn session_paths(&self, session_id: &str) -> SessionPaths {
        SessionPaths::new(self.sessions_dir().join(session_id))
    }
}

impl SessionPaths {
    /// Construct layout from an absolute session root directory.
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Session root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Home root containing this session and its profile-scoped resources.
    pub fn home_root(&self) -> PathBuf {
        self.root
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| {
                panic!(
                    "invalid session layout {}, expected .../sessions/<session_id>",
                    self.root.display()
                )
            })
            .to_path_buf()
    }

    /// Session-scoped temporary directory.
    #[must_use]
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Session-scoped persistent data directory.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Session-scoped persistent user-visible files directory.
    #[must_use]
    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    /// Session-scoped image directory for pasted images and screenshots.
    #[must_use]
    pub fn images_dir(&self) -> PathBuf {
        self.files_dir().join("images")
    }

    /// Session-scoped artifact directory for user-facing generated outputs.
    #[must_use]
    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// Session-scoped command output directory.
    #[must_use]
    pub fn outputs_dir(&self) -> PathBuf {
        self.root.join("outputs")
    }

    /// Mounted skills directory exposed to the agent sandbox.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Mounted subagents directory exposed to the agent sandbox.
    #[must_use]
    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    /// Optional task document path for long-running plans.
    #[must_use]
    pub fn tasks_path(&self) -> PathBuf {
        self.root.join("tasks.md")
    }

    /// Workspace configuration scoped to the session home/profile home.
    #[must_use]
    pub fn workspace_config(&self) -> PathBuf {
        self.home_root().join("workspace.toml")
    }
}
