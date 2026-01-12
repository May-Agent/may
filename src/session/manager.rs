//! Session management for May agent.

use jiff::Timestamp;
use petname::{Generator, Petnames};
use std::fs;
use std::path::{Path, PathBuf};

use super::{ChatHistory, SessionStore};
use crate::bundle::Bundle;
use crate::config::MayConfig;

/// Session management error.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Home directory not found")]
    NoHomeDir,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Store error: {0}")]
    Store(#[from] super::store::StoreError),
    #[error("Session not found: {0}")]
    NotFound(String),
    #[error("No active session")]
    NoActiveSession,
    #[error("Bundle error: {0}")]
    Bundle(#[from] crate::bundle::BundleError),
}

/// Unique session identifier (four random words joined by `-`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    /// Create a new random session ID using four words.
    pub fn new() -> Self {
        let petnames = Petnames::default();
        let id = petnames
            .generate_one(4, "-")
            .expect("Failed to generate session ID");
        Self(id)
    }

    /// Create from an existing string.
    pub fn from_string(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Get the ID as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0)
    }
}

/// A session representing a conversation with the agent.
pub struct Session {
    pub id: SessionId,
    pub title: Option<String>,
    /// Model bound to this session (set via /model command).
    pub model: Option<String>,
    pub history: ChatHistory,
    /// Directory where BashTool creates its working directory.
    pub sandbox_dir: PathBuf,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Whether this session has been persisted to disk.
    pub persisted: bool,
}

/// Summary of a session for listing.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: Option<String>,
    pub message_count: usize,
    pub created_at: Option<Timestamp>,
}

/// Manages sessions and their lifecycle.
pub struct SessionManager {
    may_dir: PathBuf,
    store: SessionStore,
    current: Option<Session>,
}

impl SessionManager {
    /// Create a new SessionManager, initializing ~/.may/ structure if needed.
    pub fn new() -> Result<Self, SessionError> {
        let may_dir = MayConfig::may_dir().map_err(|_| SessionError::NoHomeDir)?;
        let sessions_dir = may_dir.join("sessions");

        // Create directory structure
        fs::create_dir_all(&sessions_dir)?;

        // Ensure bundle is extracted and symlinks created
        Bundle::ensure_extracted(&may_dir)?;

        // Create memory.md if it doesn't exist
        let memory_path = may_dir.join("memory.md");
        if !memory_path.exists() {
            fs::write(&memory_path, "")?;
        }

        let store = SessionStore::new(sessions_dir);

        Ok(Self {
            may_dir,
            store,
            current: None,
        })
    }

    /// Get the May directory path.
    pub fn may_dir(&self) -> &Path {
        &self.may_dir
    }

    /// Get the sessions directory path.
    pub fn sessions_dir(&self) -> &Path {
        self.store.sessions_dir()
    }

    /// Create a new session (lazy - not persisted until first message).
    pub fn create_session(&mut self) -> Result<&Session, SessionError> {
        self.create_session_with_model(None)
    }

    /// Create a new session with a specific model.
    pub fn create_session_with_model(&mut self, model: Option<String>) -> Result<&Session, SessionError> {
        let id = SessionId::new();
        let now = Timestamp::now();

        // Don't create directories yet - defer until first message
        let sandbox_dir = self.store.sandbox_dir(id.as_str());
        let session = Session {
            id,
            title: None,
            model,
            history: ChatHistory::new(),
            sandbox_dir,
            created_at: now,
            updated_at: now,
            persisted: false,
        };

        self.current = Some(session);
        Ok(self.current.as_ref().unwrap())
    }

    /// Ensure current session is persisted to disk.
    /// Call this before writing any data to the session.
    pub async fn ensure_persisted(&mut self) -> Result<(), SessionError> {
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        if !session.persisted {
            self.store.create_session_dirs(session.id.as_str()).await?;
            session.persisted = true;
        }
        Ok(())
    }

    /// Load an existing session by ID.
    pub async fn load_session(&mut self, id: &SessionId) -> Result<&Session, SessionError> {
        if !self.store.exists(id.as_str()) {
            return Err(SessionError::NotFound(id.as_str().to_string()));
        }

        let history = self.store.read_history(id.as_str()).await?;
        let sandbox_dir = self.store.sandbox_dir(id.as_str());

        // Get created time from first message if available
        let created_at = history
            .messages()
            .first()
            .and_then(|m| m.timestamp)
            .unwrap_or_else(Timestamp::now);

        let updated_at = history
            .messages()
            .last()
            .and_then(|m| m.timestamp)
            .unwrap_or(created_at);

        // Extract metadata from history
        let title = history.metadata.title.clone();
        let model = history.metadata.model.clone();

        let session = Session {
            id: id.clone(),
            title,
            model,
            history,
            sandbox_dir,
            created_at,
            updated_at,
            persisted: true, // Loaded sessions already exist on disk
        };

        self.current = Some(session);
        Ok(self.current.as_ref().unwrap())
    }

    /// List all sessions.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError> {
        let ids = self.store.list_sessions().await?;
        let mut summaries = Vec::new();

        for id_str in ids {
            let history = self.store.read_history(&id_str).await?;
            let created_at = history.messages().first().and_then(|m| m.timestamp);

            summaries.push(SessionSummary {
                id: SessionId::from_string(id_str),
                title: history.metadata.title.clone(),
                message_count: history.len(),
                created_at,
            });
        }

        // Sort by created_at descending (newest first)
        summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(summaries)
    }

    /// Save current session state (ensures session is persisted first).
    pub async fn save_current(&mut self) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_ref().ok_or(SessionError::NoActiveSession)?;
        self.store
            .write_history(session.id.as_str(), &session.history)
            .await?;
        Ok(())
    }

    /// Set title for the current session (ensures session is persisted first).
    pub async fn set_current_title(&mut self, title: String) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        session.title = Some(title.clone());
        session.history.metadata.title = Some(title);
        // Save history with updated metadata
        self.store
            .write_history(session.id.as_str(), &session.history)
            .await?;
        Ok(())
    }

    /// Set model for the current session (ensures session is persisted first).
    pub async fn set_current_model(&mut self, model: String) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        session.model = Some(model.clone());
        session.history.metadata.model = Some(model);
        // Save history with updated metadata
        self.store
            .write_history(session.id.as_str(), &session.history)
            .await?;
        Ok(())
    }

    /// Get current session.
    pub fn current(&self) -> Option<&Session> {
        self.current.as_ref()
    }

    /// Get current session mutably.
    pub fn current_mut(&mut self) -> Option<&mut Session> {
        self.current.as_mut()
    }

    /// Get the store for direct access.
    pub fn store(&self) -> &SessionStore {
        &self.store
    }
}
