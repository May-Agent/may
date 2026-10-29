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
    /// Layout rooted at `root` (typically `~/.may`).
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Root directory of the layout.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory holding one listening socket per live main agent.
    #[must_use]
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// Directory holding per-session data.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Directory holding per-profile data.
    #[must_use]
    pub fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    /// Directory holding installed skills.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Directory holding registered subagents.
    #[must_use]
    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    /// Global memory file.
    #[must_use]
    pub fn memory_file(&self) -> PathBuf {
        self.root.join("MEMORY.md")
    }

    /// Workspace config file.
    #[must_use]
    pub fn workspace_config(&self) -> PathBuf {
        self.root.join("workspace.toml")
    }

    /// SQLite database file.
    #[must_use]
    pub fn db_file(&self) -> PathBuf {
        self.root.join("history.db")
    }

    /// Cache root for the baseline tool distribution, shared by every backend.
    #[must_use]
    pub fn tools_dir(&self) -> PathBuf {
        self.root.join("tools")
    }

    /// Hash-addressed downloaded artifacts: the `artifacts` directory
    /// `ShellSessionRegistry::initialize_tools` keeps under [`Self::tools_dir`].
    #[must_use]
    pub fn tool_artifacts_dir(&self) -> PathBuf {
        self.tools_dir().join("artifacts")
    }

    /// Prepared read-only executables: the `runtimes` directory
    /// `ShellSessionRegistry::initialize_tools` keeps under [`Self::tools_dir`].
    #[must_use]
    pub fn tool_runtimes_dir(&self) -> PathBuf {
        self.tools_dir().join("runtimes")
    }

    /// Browser runtime directory.
    #[must_use]
    pub fn browser_dir(&self) -> PathBuf {
        self.root.join("browser")
    }

    /// Browser cache directory.
    #[must_use]
    pub fn browser_cache_dir(&self) -> PathBuf {
        self.browser_dir().join("cache")
    }

    /// Browser profile (user data) directory.
    #[must_use]
    pub fn browser_profile_dir(&self) -> PathBuf {
        self.browser_dir().join("profile")
    }

    /// Path layout for one session.
    #[must_use]
    pub fn session_paths(&self, session_id: &str) -> SessionPaths {
        SessionPaths::new(self.sessions_dir().join(session_id))
    }

    /// Path layout for one profile's home dir.
    #[must_use]
    pub fn profile_home(&self, profile_id: &str) -> ProfileHomePaths {
        ProfileHomePaths {
            root: self.profiles_dir().join(profile_id),
        }
    }
}

impl ProfileHomePaths {
    /// Profile root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sessions directory under the profile root.
    #[must_use]
    pub fn sessions_dir(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Skills directory under the profile root.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Subagents directory under the profile root.
    #[must_use]
    pub fn subagents_dir(&self) -> PathBuf {
        self.root.join("subagents")
    }

    /// Workspace config under the profile root.
    #[must_use]
    pub fn workspace_config(&self) -> PathBuf {
        self.root.join("workspace.toml")
    }

    /// Path layout for one session under this profile.
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
    ///
    /// # Panics
    /// Panics when the profile root has no parent (internal invariant).
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

    /// Contents of the session's `tasks.md`; `Ok(None)` when absent.
    ///
    /// The file is the canonical task document — the model edits it inside
    /// the session sandbox and every reader (run loop, REST surface, TUI)
    /// goes through this read so all of them observe the same bytes.
    ///
    /// # Errors
    /// Returns the underlying IO error when the read fails for a reason
    /// other than the file being absent.
    pub async fn tasks_content(&self) -> std::io::Result<Option<String>> {
        match async_fs::read_to_string(self.tasks_path()).await {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Workspace configuration scoped to the session home/profile home.
    #[must_use]
    pub fn workspace_config(&self) -> PathBuf {
        self.home_root().join("workspace.toml")
    }
}
