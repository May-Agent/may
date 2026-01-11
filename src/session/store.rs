//! Filesystem persistence for sessions.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ChatHistory, ChatHistoryError};

/// Session metadata stored in metadata.json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Session title (generated after first response).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Model bound to this session (set via /model command).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Session store error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Chat history error: {0}")]
    History(#[from] ChatHistoryError),
}

/// Handles persistence of sessions to filesystem.
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

    /// Get path for session metadata file.
    pub fn metadata_path(&self, id: &str) -> PathBuf {
        self.session_dir(id).join("metadata.json")
    }

    /// Check if a session exists.
    pub fn exists(&self, id: &str) -> bool {
        self.session_dir(id).exists()
    }

    /// Create session directory structure.
    pub fn create_session_dirs(&self, id: &str) -> Result<(), StoreError> {
        let session_dir = self.session_dir(id);
        fs::create_dir_all(&session_dir)?;
        fs::create_dir_all(self.images_dir(id))?;

        // Create sandbox symlinks to global resources
        let may_dir = self.sessions_dir.parent().unwrap_or(&self.sessions_dir);
        crate::bundle::Bundle::create_sandbox_symlinks(&session_dir, may_dir)
            .map_err(|e| StoreError::Io(std::io::Error::other(e.to_string())))?;

        Ok(())
    }

    /// Read chat history from disk.
    pub fn read_history(&self, id: &str) -> Result<ChatHistory, StoreError> {
        let path = self.chat_path(id);
        if !path.exists() {
            return Ok(ChatHistory::new());
        }
        let content = fs::read_to_string(&path)?;
        Ok(ChatHistory::parse(&content)?)
    }

    /// Write chat history to disk.
    pub fn write_history(&self, id: &str, history: &ChatHistory) -> Result<(), StoreError> {
        let path = self.chat_path(id);
        let content = history.serialize();
        fs::write(&path, content)?;
        Ok(())
    }

    /// List all session IDs.
    pub fn list_sessions(&self) -> Result<Vec<String>, StoreError> {
        let mut sessions = Vec::new();

        if !self.sessions_dir.exists() {
            return Ok(sessions);
        }

        for entry in fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    sessions.push(name.to_string());
                }
            }
        }

        Ok(sessions)
    }

    /// Delete a session.
    pub fn delete_session(&self, id: &str) -> Result<(), StoreError> {
        let path = self.session_dir(id);
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        Ok(())
    }

    /// Read session metadata from disk.
    pub fn read_metadata(&self, id: &str) -> SessionMetadata {
        let path = self.metadata_path(id);
        if !path.exists() {
            return SessionMetadata::default();
        }
        fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    /// Write session metadata to disk.
    pub fn write_metadata(&self, id: &str, metadata: &SessionMetadata) -> Result<(), StoreError> {
        let path = self.metadata_path(id);
        let content = serde_json::to_string_pretty(metadata)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        fs::write(&path, content)?;
        Ok(())
    }
}
