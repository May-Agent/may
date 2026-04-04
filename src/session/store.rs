//! Filesystem persistence for sessions.

use std::path::{Path, PathBuf};

use blocking::unblock;
use futures_lite::StreamExt;

use super::{MayHomePaths, SessionPaths};
use crate::fs_utils::copy_dir_contents_recursive;

/// Session store error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Bundle error: {0}")]
    Bundle(#[from] crate::bundle::BundleError),
}

/// Handles persistence of sessions to filesystem.
#[derive(Clone)]
pub struct SessionStore {
    home_paths: MayHomePaths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxResourceLayout {
    Symlinked,
    ContainerMountPoints,
}

impl SessionStore {
    /// Create a new session store.
    #[must_use]
    pub const fn new(home_paths: MayHomePaths) -> Self {
        Self { home_paths }
    }

    /// Get path for a session directory.
    #[must_use]
    pub fn session_dir(&self, id: &str, profile_id: Option<&str>) -> PathBuf {
        self.session_paths(id, profile_id).root().to_path_buf()
    }

    /// Get typed layout for a session.
    #[must_use]
    pub fn session_paths(&self, id: &str, profile_id: Option<&str>) -> SessionPaths {
        match profile_id {
            Some(profile_id) => self.home_paths.profile_home(profile_id).session_paths(id),
            None => self.home_paths.session_paths(id),
        }
    }

    /// Get path for a session's images directory.
    #[must_use]
    pub fn images_dir(&self, id: &str, profile_id: Option<&str>) -> PathBuf {
        self.session_paths(id, profile_id).images_dir()
    }

    /// Get path for a session's sandbox directory.
    /// `TerminalTool` will create its own working directory inside this.
    #[must_use]
    pub fn sandbox_dir(&self, id: &str, profile_id: Option<&str>) -> PathBuf {
        self.session_dir(id, profile_id)
    }

    /// Check if a session exists.
    #[must_use]
    pub fn exists(&self, id: &str, profile_id: Option<&str>) -> bool {
        self.session_dir(id, profile_id).exists()
    }

    /// Create session directory structure.
    pub async fn create_session_dirs(
        &self,
        id: &str,
        profile_id: Option<&str>,
        layout: SandboxResourceLayout,
    ) -> Result<(), StoreError> {
        let paths = self.session_paths(id, profile_id);
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
                let symlink_root = match profile_id {
                    Some(profile_id) => {
                        let profile_home = self.home_paths.profile_home(profile_id);
                        async_fs::create_dir_all(profile_home.root()).await?;
                        async_fs::create_dir_all(profile_home.skills_dir()).await?;
                        async_fs::create_dir_all(profile_home.subagents_dir()).await?;
                        self.seed_profile_scoped_resources(profile_id).await?;
                        profile_home.root().to_path_buf()
                    }
                    None => self.home_paths.root().to_path_buf(),
                };
                crate::bundle::Bundle::create_sandbox_home_symlinks(paths.root(), &symlink_root)
                    .await?;
            }
            SandboxResourceLayout::ContainerMountPoints => {
                Self::ensure_mount_point_dir(paths.root(), "skills").await?;
                Self::ensure_mount_point_dir(paths.root(), "subagents").await?;
                self.seed_session_scoped_resources(&paths, profile_id)
                    .await?;
            }
        }

        Ok(())
    }

    pub async fn reconcile_container_mount_points(
        &self,
        id: &str,
        profile_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let paths = self.session_paths(id, profile_id);
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
        self.seed_session_scoped_resources(&paths, profile_id)
            .await?;
        Ok(())
    }

    /// Delete a session.
    pub async fn delete_session(
        &self,
        id: &str,
        profile_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let path = self.session_dir(id, profile_id);
        if path.exists() {
            async_fs::remove_dir_all(&path).await?;
        }
        Ok(())
    }

    /// Delete every session directory across the root home and all profile homes.
    pub async fn delete_all_session_dirs(&self) -> Result<(), StoreError> {
        let sessions_dir = self.home_paths.sessions_dir();
        if sessions_dir.exists() {
            async_fs::remove_dir_all(&sessions_dir).await?;
            async_fs::create_dir_all(&sessions_dir).await?;
        }

        let profiles_dir = self.home_paths.profiles_dir();
        if async_fs::metadata(&profiles_dir).await.is_ok() {
            let mut entries = async_fs::read_dir(&profiles_dir).await?;
            while let Some(entry) = entries.next().await {
                let entry = entry?;
                if entry.file_type().await?.is_dir() {
                    let sessions_dir = entry.path().join("sessions");
                    if async_fs::metadata(&sessions_dir).await.is_ok() {
                        async_fs::remove_dir_all(&sessions_dir).await?;
                    }
                }
            }
        }

        Ok(())
    }

    async fn ensure_mount_point_dir(parent: &Path, name: &str) -> Result<(), StoreError> {
        let target = parent.join(name);
        match async_fs::symlink_metadata(&target).await {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    tracing::debug!(path = %target.display(), "Removing stale symlink mount point");
                    async_fs::remove_file(&target).await?;
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

    async fn seed_profile_scoped_resources(&self, profile_id: &str) -> Result<(), StoreError> {
        let profile_home = self.home_paths.profile_home(profile_id);
        Self::sync_dir_contents(&self.home_paths.skills_dir(), &profile_home.skills_dir()).await?;
        Self::sync_dir_contents(
            &self.home_paths.subagents_dir(),
            &profile_home.subagents_dir(),
        )
        .await?;
        Ok(())
    }

    pub async fn sync_profile_scoped_resources(&self, profile_id: &str) -> Result<(), StoreError> {
        self.seed_profile_scoped_resources(profile_id).await
    }

    async fn seed_session_scoped_resources(
        &self,
        paths: &SessionPaths,
        profile_id: Option<&str>,
    ) -> Result<(), StoreError> {
        let (skills_dir, subagents_dir) = match profile_id {
            Some(profile_id) => {
                self.seed_profile_scoped_resources(profile_id).await?;
                let profile_home = self.home_paths.profile_home(profile_id);
                (profile_home.skills_dir(), profile_home.subagents_dir())
            }
            None => (
                self.home_paths.skills_dir(),
                self.home_paths.subagents_dir(),
            ),
        };
        Self::sync_dir_contents(&skills_dir, &paths.skills_dir()).await?;
        Self::sync_dir_contents(&subagents_dir, &paths.subagents_dir()).await?;
        Ok(())
    }

    async fn sync_dir_contents(source: &Path, target: &Path) -> Result<(), StoreError> {
        if async_fs::metadata(source).await.is_err() {
            return Ok(());
        }
        let source = source.to_path_buf();
        let target = target.to_path_buf();
        unblock(move || copy_dir_contents_recursive(&source, &target)).await?;
        Ok(())
    }
}
