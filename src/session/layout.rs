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
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    pub fn memory_file(&self) -> PathBuf {
        self.root.join("MEMORY.md")
    }

    pub fn workspace_config(&self) -> PathBuf {
        self.root.join("workspace.toml")
    }

    pub fn db_file(&self) -> PathBuf {
        self.root.join("history.db")
    }

    pub fn session_paths(&self, session_id: &str) -> SessionPaths {
        SessionPaths::new(self.sessions_dir().join(session_id))
    }

    pub fn profile_home(&self, profile_id: &str) -> ProfileHomePaths {
        ProfileHomePaths {
            root: self.profiles_dir().join(profile_id),
        }
    }
}

impl ProfileHomePaths {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    pub fn workspace_config(&self) -> PathBuf {
        self.root.join("workspace.toml")
    }

    pub fn session_paths(&self, session_id: &str) -> SessionPaths {
        SessionPaths::new(self.sessions_dir().join(session_id))
    }
}

impl SessionPaths {
    /// Construct layout from an absolute session root directory.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Session root directory.
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
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Session-scoped persistent data directory.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// Session-scoped persistent user-visible files directory.
    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    /// Session-scoped image directory for pasted images and screenshots.
    pub fn images_dir(&self) -> PathBuf {
        self.files_dir().join("images")
    }

    /// Session-scoped artifact directory for user-facing generated outputs.
    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    /// Session-scoped command output directory.
    pub fn outputs_dir(&self) -> PathBuf {
        self.root.join("outputs")
    }

    /// Mounted skills directory exposed to the agent sandbox.
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Mounted subagents directory exposed to the agent sandbox.
    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    /// Human-readable chat transcript backup path.
    pub fn chat_backup_path(&self) -> PathBuf {
        self.data_dir().join("chat.txt")
    }

    /// Long-lived agent transcript path used for compaction recovery.
    pub fn transcript_path(&self) -> PathBuf {
        self.data_dir().join("transcript.md")
    }

    /// Optional task document path for long-running plans.
    pub fn tasks_path(&self) -> PathBuf {
        self.root.join("tasks.md")
    }

    /// Workspace configuration scoped to the session home/profile home.
    pub fn workspace_config(&self) -> PathBuf {
        self.home_root().join("workspace.toml")
    }
}
