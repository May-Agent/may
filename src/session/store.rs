//! Filesystem persistence for sessions.

use std::path::{Path, PathBuf};

use futures_lite::StreamExt;

use super::{ChatHistory, ChatHistoryError, SessionPaths};
use crate::fs_utils::copy_dir_contents_recursive;

/// Session store error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Chat history error: {0}")]
    History(#[from] ChatHistoryError),
    #[error("Bundle error: {0}")]
    Bundle(#[from] crate::bundle::BundleError),
}

/// Handles persistence of sessions to filesystem.
#[derive(Clone)]
pub struct SessionStore {
    may_dir: PathBuf,
    sessions_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxResourceLayout {
    Symlinked,
    ContainerMountPoints,
}

impl SessionStore {
    /// Create a new session store.
    pub fn new(may_dir: PathBuf, sessions_dir: PathBuf) -> Self {
        Self {
            may_dir,
            sessions_dir,
        }
    }

    /// Get the sessions directory.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Get path for a session directory.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(id)
    }

    /// Get typed layout for a session.
    pub fn session_paths(&self, id: &str) -> SessionPaths {
        SessionPaths::new(self.session_dir(id))
    }

    /// Get path for a session's chat history.
    pub fn chat_path(&self, id: &str) -> PathBuf {
        self.session_paths(id).chat_backup_path()
    }

    /// Get path for a session's images directory.
    pub fn images_dir(&self, id: &str) -> PathBuf {
        self.session_paths(id).images_dir()
    }

    /// Get path for a session's sandbox directory.
    /// BashTool will create its own working directory inside this.
    pub fn sandbox_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id)
    }

    /// Check if a session exists.
    pub fn exists(&self, id: &str) -> bool {
        self.session_dir(id).exists()
    }

    /// Create session directory structure.
    pub async fn create_session_dirs(
        &self,
        id: &str,
        layout: SandboxResourceLayout,
    ) -> Result<(), StoreError> {
        let paths = self.session_paths(id);
        async_fs::create_dir_all(paths.root()).await?;
        async_fs::create_dir_all(paths.tmp_dir()).await?;
        async_fs::create_dir_all(paths.data_dir()).await?;
        async_fs::create_dir_all(paths.files_dir()).await?;
        async_fs::create_dir_all(paths.images_dir()).await?;
        async_fs::create_dir_all(paths.outputs_dir()).await?;
        async_fs::create_dir_all(paths.artifacts_dir()).await?;

        match layout {
            SandboxResourceLayout::Symlinked => {
                // Create sandbox symlinks to global resources.
                crate::bundle::Bundle::create_sandbox_symlinks(paths.root(), &self.may_dir).await?;
            }
            SandboxResourceLayout::ContainerMountPoints => {
                Self::ensure_mount_point_dir(paths.root(), "skills").await?;
                Self::ensure_mount_point_dir(paths.root(), "subagents").await?;
                self.seed_session_scoped_resources(&paths).await?;
            }
        }

        Ok(())
    }

    pub async fn reconcile_container_mount_points(&self, id: &str) -> Result<(), StoreError> {
        let paths = self.session_paths(id);
        if async_fs::metadata(paths.root()).await.is_err() {
            return Ok(());
        }
        tracing::debug!(session_id = %id, path = %paths.root().display(), "Reconciling container mount points for session");

        async_fs::create_dir_all(paths.tmp_dir()).await?;
        async_fs::create_dir_all(paths.data_dir()).await?;
        async_fs::create_dir_all(paths.files_dir()).await?;
        async_fs::create_dir_all(paths.images_dir()).await?;
        async_fs::create_dir_all(paths.outputs_dir()).await?;
        async_fs::create_dir_all(paths.artifacts_dir()).await?;
        Self::ensure_mount_point_dir(paths.root(), "skills").await?;
        Self::ensure_mount_point_dir(paths.root(), "subagents").await?;
        self.seed_session_scoped_resources(&paths).await?;
        Ok(())
    }

    /// Read chat history from disk.
    pub async fn read_history(&self, id: &str) -> Result<ChatHistory, StoreError> {
        let path = self.chat_path(id);
        if async_fs::metadata(&path).await.is_err() {
            return Ok(ChatHistory::new());
        }
        let content = async_fs::read_to_string(&path).await?;
        Ok(ChatHistory::parse(&content)?)
    }

    /// Write chat history to disk.
    pub async fn write_history(&self, id: &str, history: &ChatHistory) -> Result<(), StoreError> {
        let path = self.chat_path(id);
        let content = history.serialize();
        async_fs::write(&path, content).await?;
        Ok(())
    }

    /// List all session IDs.
    pub async fn list_sessions(&self) -> Result<Vec<String>, StoreError> {
        let mut sessions = Vec::new();

        if !self.sessions_dir.exists() {
            return Ok(sessions);
        }

        let mut entries = async_fs::read_dir(&self.sessions_dir).await?;
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            if entry.file_type().await?.is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                sessions.push(name.to_string());
            }
        }

        Ok(sessions)
    }

    /// Delete a session.
    pub async fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        let path = self.session_dir(id);
        if path.exists() {
            async_fs::remove_dir_all(&path).await?;
        }
        Ok(())
    }

    async fn ensure_mount_point_dir(parent: &Path, name: &str) -> Result<(), StoreError> {
        let target = parent.join(name);
        match std::fs::symlink_metadata(&target) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    tracing::debug!(path = %target.display(), "Removing stale symlink mount point");
                    std::fs::remove_file(&target)?;
                } else if meta.is_dir() {
                    return Ok(());
                } else {
                    return Err(StoreError::Io(std::io::Error::other(format!(
                        "invalid container mount point {}, expected directory",
                        target.display()
                    ))));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Io(error)),
        }

        async_fs::create_dir_all(&target).await?;
        Ok(())
    }

    async fn seed_session_scoped_resources(&self, paths: &SessionPaths) -> Result<(), StoreError> {
        Self::copy_dir_contents_if_target_empty(&self.may_dir.join("skills"), &paths.skills_dir())
            .await?;
        Self::copy_dir_contents_if_target_empty(
            &self.may_dir.join("subagents"),
            &paths.subagents_dir(),
        )
        .await?;
        Ok(())
    }

    async fn copy_dir_contents_if_target_empty(
        source: &Path,
        target: &Path,
    ) -> Result<(), StoreError> {
        if async_fs::metadata(source).await.is_err() {
            return Ok(());
        }
        if !Self::is_dir_empty(target).await? {
            return Ok(());
        }
        let source = source.to_path_buf();
        let target = target.to_path_buf();
        tokio::task::spawn_blocking(move || copy_dir_contents_recursive(&source, &target))
            .await
            .map_err(|error| StoreError::Io(std::io::Error::other(error.to_string())))??;
        Ok(())
    }

    async fn is_dir_empty(path: &Path) -> Result<bool, StoreError> {
        let mut entries = async_fs::read_dir(path).await?;
        Ok(entries.next().await.is_none())
    }
}
