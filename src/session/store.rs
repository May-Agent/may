//! Filesystem persistence for sessions.

use std::path::{Path, PathBuf};

use futures_lite::StreamExt;

use super::{ChatHistory, ChatHistoryError};
use crate::fs_util;

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
    sessions_dir: PathBuf,
}

impl SessionStore {
    /// Create a new session store.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self { sessions_dir }
    }

    /// Get the sessions directory.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Get path for a session directory.
    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(id)
    }

    /// Get path for a session's chat history.
    pub fn chat_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("chat.txt")
    }

    /// Get path for a session's images directory.
    pub fn images_dir(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("images")
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
    pub async fn create_session_dirs(&self, id: &str) -> Result<(), StoreError> {
        let session_dir = self.session_dir(id);
        async_fs::create_dir_all(&session_dir).await?;
        async_fs::create_dir_all(self.images_dir(id)).await?;

        // Create sandbox symlinks to global resources
        let may_dir = self.sessions_dir.parent().unwrap_or(&self.sessions_dir).to_path_buf();
        crate::bundle::Bundle::create_sandbox_symlinks(&session_dir, &may_dir).await?;

        Ok(())
    }

    /// Read chat history from disk.
    pub async fn read_history(&self, id: &str) -> Result<ChatHistory, StoreError> {
        let path = self.chat_path(id);
        if async_fs::metadata(&path).await.is_err() {
            return Ok(ChatHistory::new());
        }
        let content = fs_util::read_to_string(&path).await?;
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
            if entry.file_type().await?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    sessions.push(name.to_string());
                }
            }
        }

        Ok(sessions)
    }

    /// List all sessions with summaries (for background loading).
    pub async fn list_session_summaries(
        &self,
    ) -> Result<Vec<super::manager::SessionSummary>, StoreError> {
        use super::manager::{SessionId, SessionSummary};

        let ids = self.list_sessions().await?;
        let mut summaries = Vec::new();

        for id_str in ids {
            let history = self.read_history(&id_str).await?;
            let created_at = history.messages().first().and_then(|m| m.timestamp);

            // Build content preview from messages for search
            let content_preview: String = history
                .messages()
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join(" ");

            summaries.push(SessionSummary {
                id: SessionId::from_string(id_str),
                title: history.metadata.title.clone(),
                message_count: history.len(),
                created_at,
                content_preview,
            });
        }

        // Sort by created_at descending (newest first)
        summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(summaries)
    }

    /// Delete a session.
    pub async fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        let path = self.session_dir(id);
        if path.exists() {
            async_fs::remove_dir_all(&path).await?;
        }
        Ok(())
    }
}
