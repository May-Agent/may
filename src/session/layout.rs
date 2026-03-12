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
}

impl SessionPaths {
    /// Construct layout from an absolute session root directory.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Construct layout from the May directory and session id.
    pub fn from_may_dir(may_dir: &Path, session_id: &str) -> Self {
        Self::new(may_dir.join("sessions").join(session_id))
    }

    /// Construct layout from typed May home paths and session id.
    pub fn from_home_paths(home_paths: &MayHomePaths, session_id: &str) -> Self {
        home_paths.session_paths(session_id)
    }

    /// Session root directory.
    pub fn root(&self) -> &Path {
        &self.root
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
}
