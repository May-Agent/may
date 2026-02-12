//! Session management for May agent.

use jiff::Timestamp;
use petname::{Generator, Petnames};
use std::fs;
use std::path::{Path, PathBuf};

use super::{ChatHistory, ChatMessage, MessageRole, SessionStore};
use crate::bundle::Bundle;
use crate::config::MayConfig;
use crate::db::{BulkMessage, ChatDb, ChatDbError};

/// Session management error.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Home directory not found")]
    NoHomeDir,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Store error: {0}")]
    Store(#[from] super::store::StoreError),
    #[error("Database error: {0}")]
    Database(#[from] ChatDbError),
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

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
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
    pub model: Option<String>,
    pub message_count: usize,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    /// Content preview for searching (concatenated messages).
    pub content_preview: String,
}

impl SessionSummary {
    /// Convert a DB session model to a summary.
    pub fn from_db_model(s: crate::db::entity::sessions::Model) -> Self {
        let created_at = Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", &s.created_at).ok();
        let updated_at = Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", &s.updated_at).ok();

        Self {
            id: SessionId::from_string(s.id),
            title: s.title,
            model: s.model,
            message_count: s.message_count as usize,
            created_at,
            updated_at,
            content_preview: s.preview.unwrap_or_default(),
        }
    }
}

/// List all session summaries from a [`ChatDb`] handle.
///
/// This is a standalone function that doesn't require a [`SessionManager`] lock,
/// useful for spawned tasks that only need a `ChatDb` clone.
pub async fn list_summaries_from_db(db: &ChatDb) -> Result<Vec<SessionSummary>, SessionError> {
    let db_sessions = db.list_sessions().await?;
    Ok(db_sessions
        .into_iter()
        .map(SessionSummary::from_db_model)
        .collect())
}

/// Manages sessions and their lifecycle.
pub struct SessionManager {
    may_dir: PathBuf,
    store: SessionStore,
    db: ChatDb,
    current: Option<Session>,
}

impl SessionManager {
    /// Create a new SessionManager, initializing ~/.may/ structure if needed.
    ///
    /// Opens the SQLite database and migrates any legacy `chat.txt` sessions.
    pub async fn new() -> Result<Self, SessionError> {
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

        let store = SessionStore::new(may_dir.clone(), sessions_dir);

        // Open the database
        let db_path = may_dir.join("history.db");
        let db = ChatDb::open(&db_path).await?;

        let manager = Self {
            may_dir,
            store,
            db,
            current: None,
        };

        // Migrate legacy chat.txt sessions into the database
        manager.migrate_legacy_sessions().await;

        Ok(manager)
    }

    /// Migrate legacy `chat.txt` sessions into the database.
    ///
    /// For each session directory, if the session doesn't exist in the DB
    /// but has a `chat.txt`, import it. This is idempotent.
    async fn migrate_legacy_sessions(&self) {
        let session_ids = match self.store.list_sessions().await {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!("Failed to list legacy sessions for migration: {}", e);
                return;
            }
        };

        let mut migrated = 0;
        for id_str in &session_ids {
            // Skip if already in DB
            match self.db.session_exists(id_str).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("Failed to check session {} in DB: {}", id_str, e);
                    continue;
                }
            }

            // Read legacy chat.txt
            let history = match self.store.read_history(id_str).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("Failed to read legacy history for {}: {}", id_str, e);
                    continue;
                }
            };

            // Determine created_at from first message
            let created_at = history
                .messages()
                .first()
                .and_then(|m| m.timestamp)
                .map(|t| t.to_string())
                .unwrap_or_else(|| jiff::Timestamp::now().to_string());

            // Create session in DB
            if let Err(e) = self
                .db
                .create_session(
                    id_str,
                    history.metadata.title.as_deref(),
                    history.metadata.model.as_deref(),
                )
                .await
            {
                tracing::warn!("Failed to create DB session for {}: {}", id_str, e);
                continue;
            }

            // Set the created_at properly (create_session uses now())
            if let Err(e) = self.db.set_session_created_at(id_str, &created_at).await {
                tracing::warn!("Failed to set created_at for {}: {}", id_str, e);
            }

            // We need timestamps as owned strings for the borrow checker
            let timestamps: Vec<Option<String>> = history
                .messages()
                .iter()
                .map(|m| m.timestamp.map(|t| t.to_string()))
                .collect();

            let bulk_messages: Vec<BulkMessage<'_>> = history
                .messages()
                .iter()
                .zip(timestamps.iter())
                .map(|(m, ts)| BulkMessage {
                    role: m.role.as_str(),
                    content: &m.content,
                    timestamp: ts.as_deref(),
                    tool_call_id: m.tool_call_id.as_deref(),
                })
                .collect();

            if let Err(e) = self.db.bulk_insert_messages(id_str, &bulk_messages).await {
                tracing::warn!("Failed to migrate messages for {}: {}", id_str, e);
                continue;
            }

            migrated += 1;
        }

        if migrated > 0 {
            tracing::info!("Migrated {} legacy sessions to database", migrated);
        }
    }

    /// Get the May directory path.
    pub fn may_dir(&self) -> &Path {
        &self.may_dir
    }

    /// Get the sessions directory path.
    pub fn sessions_dir(&self) -> &Path {
        self.store.sessions_dir()
    }

    /// Get the database handle.
    pub fn db(&self) -> &ChatDb {
        &self.db
    }

    /// Create a new session (lazy - not persisted until first message).
    pub fn create_session(&mut self) -> Result<&Session, SessionError> {
        self.create_session_with_model(None)
    }

    /// Create a new session with a specific model.
    pub fn create_session_with_model(
        &mut self,
        model: Option<String>,
    ) -> Result<&Session, SessionError> {
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
        // SAFETY: We just set `self.current` to `Some` above.
        Ok(self.current.as_ref().expect("just assigned"))
    }

    /// Ensure current session is persisted to disk and database.
    /// Call this before writing any data to the session.
    pub async fn ensure_persisted(&mut self) -> Result<(), SessionError> {
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        if !session.persisted {
            // Create filesystem directories (for sandbox, images, etc.)
            self.store.create_session_dirs(session.id.as_str()).await?;

            // Create session in database
            self.db
                .create_session(
                    session.id.as_str(),
                    session.title.as_deref(),
                    session.model.as_deref(),
                )
                .await?;

            session.persisted = true;
        }
        Ok(())
    }

    /// Load an existing session by ID (from database).
    pub async fn load_session(&mut self, id: &SessionId) -> Result<&Session, SessionError> {
        // Try DB first
        let db_session = self.db.get_session(id.as_str()).await?;

        let (title, model, created_at, updated_at) = if let Some(ref s) = db_session {
            (
                s.title.clone(),
                s.model.clone(),
                Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", &s.created_at)
                    .unwrap_or_else(|_| Timestamp::now()),
                Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", &s.updated_at)
                    .unwrap_or_else(|_| Timestamp::now()),
            )
        } else {
            // Fallback: check filesystem
            if !self.store.exists(id.as_str()) {
                return Err(SessionError::NotFound(id.as_str().to_string()));
            }
            // The session exists on disk but not in DB (shouldn't happen after migration)
            let history = self.store.read_history(id.as_str()).await?;
            let ca = history
                .messages()
                .first()
                .and_then(|m| m.timestamp)
                .unwrap_or_else(Timestamp::now);
            let ua = history
                .messages()
                .last()
                .and_then(|m| m.timestamp)
                .unwrap_or(ca);
            (
                history.metadata.title.clone(),
                history.metadata.model.clone(),
                ca,
                ua,
            )
        };

        // Load messages from DB to build ChatHistory
        let history = if db_session.is_some() {
            self.load_history_from_db(id.as_str()).await?
        } else {
            self.store.read_history(id.as_str()).await?
        };

        let sandbox_dir = self.store.sandbox_dir(id.as_str());
        let session = Session {
            id: id.clone(),
            title,
            model,
            history,
            sandbox_dir,
            created_at,
            updated_at,
            persisted: true,
        };

        self.current = Some(session);
        Ok(self.current.as_ref().expect("just assigned"))
    }

    /// Build a `ChatHistory` from database messages.
    async fn load_history_from_db(&self, session_id: &str) -> Result<ChatHistory, SessionError> {
        let db_session = self.db.get_session(session_id).await?;
        let db_messages = self.db.get_messages(session_id).await?;

        let mut history = ChatHistory::new();

        // Set metadata from session record
        if let Some(s) = db_session {
            history.metadata.title = s.title;
            history.metadata.model = s.model;
        }

        // Convert DB message rows to ChatMessage
        for msg in db_messages {
            let role = MessageRole::from_str(&msg.role).unwrap_or(MessageRole::User);
            let timestamp = msg
                .timestamp
                .as_deref()
                .and_then(|t| Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", t).ok());

            history.push(ChatMessage {
                role,
                content: msg.content,
                timestamp,
                tool_call_id: msg.tool_call_id,
            });
        }

        Ok(history)
    }

    /// Append a message to the current session (DB + in-memory).
    pub async fn append_message(&mut self, message: ChatMessage) -> Result<(), SessionError> {
        self.ensure_persisted().await?;

        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        let session_id = session.id.as_str().to_string();

        let ts_string = message.timestamp.map(|t| t.to_string());

        // Write to database
        self.db
            .append_message(
                &session_id,
                message.role.as_str(),
                &message.content,
                ts_string.as_deref(),
                message.tool_call_id.as_deref(),
            )
            .await?;

        // Update in-memory history
        session.history.push(message);

        Ok(())
    }

    /// List all sessions (from database).
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError> {
        list_summaries_from_db(&self.db).await
    }

    /// Save current session state to database.
    pub async fn save_current(&mut self) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        // With DB-backed storage, individual messages are already persisted via append_message.
        // This method now only needs to sync metadata and write the backup chat.txt.
        let session = self.current.as_ref().ok_or(SessionError::NoActiveSession)?;

        // Write chat.txt as human-readable backup (non-fatal)
        if let Err(e) = self
            .store
            .write_history(session.id.as_str(), &session.history)
            .await
        {
            tracing::warn!("Failed to write chat.txt backup: {}", e);
        }

        Ok(())
    }

    /// Set title for the current session.
    pub async fn set_current_title(&mut self, title: String) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        session.title = Some(title.clone());
        session.history.metadata.title = Some(title.clone());

        // Update database
        self.db
            .update_session(session.id.as_str(), Some(&title), None)
            .await?;

        // Write chat.txt backup (non-fatal)
        if let Err(e) = self
            .store
            .write_history(session.id.as_str(), &session.history)
            .await
        {
            tracing::warn!("Failed to write chat.txt backup: {}", e);
        }

        Ok(())
    }

    /// Set model for the current session.
    pub async fn set_current_model(&mut self, model: String) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        session.model = Some(model.clone());
        session.history.metadata.model = Some(model.clone());

        // Update database
        self.db
            .update_session(session.id.as_str(), None, Some(&model))
            .await?;

        // Write chat.txt backup (non-fatal)
        if let Err(e) = self
            .store
            .write_history(session.id.as_str(), &session.history)
            .await
        {
            tracing::warn!("Failed to write chat.txt backup: {}", e);
        }

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

    /// Delete a session from both DB and filesystem.
    pub async fn delete_session(&mut self, id: &str) -> Result<(), SessionError> {
        self.db.delete_session(id).await?;
        self.store.delete_session(id).await?;
        Ok(())
    }
}
