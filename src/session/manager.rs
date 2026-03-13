//! Session management for May agent.

use jiff::Timestamp;
use petname::{Generator, Petnames};
use std::fs;
use std::path::{Path, PathBuf};

use super::{MayHomePaths, SessionRuntimeSummary};
use super::store::SandboxResourceLayout;
use super::{ChatHistory, ChatMessage, ChatMessageStats, MessageRole, SessionStore};
use crate::bundle::Bundle;
use crate::config::MayConfig;
use crate::db::{ChatDb, ChatDbError};

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
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
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
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    pub history: ChatHistory,
    /// Directory where BashTool creates its working directory.
    pub sandbox_dir: PathBuf,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    /// Whether this session has been persisted to disk.
    pub persisted: bool,
}

/// Summary of a session for listing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: Option<String>,
    pub model: Option<String>,
    pub profile_id: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub folder_id: Option<String>,
    pub pin_sort_order: Option<i32>,
    pub message_count: usize,
    pub created_at: Option<Timestamp>,
    pub updated_at: Option<Timestamp>,
    /// Content preview for searching (concatenated messages).
    pub content_preview: String,
    /// Persisted runtime summary for refresh-safe UI state.
    pub runtime: SessionRuntimeSummary,
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
            profile_id: s.profile_id,
            pinned: s.pinned,
            archived: s.archived,
            folder_id: s.folder_id,
            pin_sort_order: s.pin_sort_order,
            message_count: s.message_count as usize,
            created_at,
            updated_at,
            content_preview: s.preview.unwrap_or_default(),
            runtime: SessionRuntimeSummary::default(),
        }
    }
}

fn map_i32_to_u32(value: Option<i32>) -> Option<u32> {
    value.and_then(|v| u32::try_from(v).ok())
}

fn map_i64_to_u64(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok())
}

fn map_db_message_stats(row: &crate::db::entity::messages::Model) -> Option<ChatMessageStats> {
    let stats = ChatMessageStats {
        provider_id: row.provider_id.clone(),
        prompt_tokens: map_i32_to_u32(row.prompt_tokens),
        completion_tokens: map_i32_to_u32(row.completion_tokens),
        total_tokens: map_i32_to_u32(row.total_tokens),
        cache_read_tokens: map_i32_to_u32(row.cache_read_tokens),
        cache_write_tokens: map_i32_to_u32(row.cache_write_tokens),
        context_window: map_i32_to_u32(row.context_window),
        elapsed_ms: map_i64_to_u64(row.elapsed_ms),
    };
    if stats.provider_id.is_none()
        && stats.prompt_tokens.is_none()
        && stats.completion_tokens.is_none()
        && stats.total_tokens.is_none()
        && stats.cache_read_tokens.is_none()
        && stats.cache_write_tokens.is_none()
        && stats.context_window.is_none()
        && stats.elapsed_ms.is_none()
    {
        None
    } else {
        Some(stats)
    }
}

fn map_chat_stats_to_db_params(stats: &ChatMessageStats) -> crate::db::MessageStatsParams {
    crate::db::MessageStatsParams {
        provider_id: stats.provider_id.clone(),
        prompt_tokens: stats.prompt_tokens,
        completion_tokens: stats.completion_tokens,
        total_tokens: stats.total_tokens,
        cache_read_tokens: stats.cache_read_tokens,
        cache_write_tokens: stats.cache_write_tokens,
        context_window: stats.context_window,
        elapsed_ms: stats.elapsed_ms,
    }
}

/// List all session summaries from a [`ChatDb`] handle.
///
/// This is a standalone function that doesn't require a [`SessionManager`] lock,
/// useful for spawned tasks that only need a `ChatDb` clone.
pub async fn list_summaries_from_db(db: &ChatDb) -> Result<Vec<SessionSummary>, SessionError> {
    let db_sessions = db.list_sessions().await?;
    let mut summaries = Vec::with_capacity(db_sessions.len());
    for session in db_sessions {
        let mut summary = SessionSummary::from_db_model(session);
        summary.runtime = db
            .get_session_runtime(summary.id.as_str())
            .await?
            .unwrap_or_default();
        summaries.push(summary);
    }
    Ok(summaries)
}

/// Manages sessions and their lifecycle.
pub struct SessionManager {
    may_dir: PathBuf,
    store: SessionStore,
    db: ChatDb,
    current: Option<Session>,
    /// The currently active profile ID (sessions are created within this profile).
    current_profile_id: Option<String>,
}

impl SessionManager {
    /// Create a new SessionManager, initializing ~/.may/ structure if needed.
    pub async fn new() -> Result<Self, SessionError> {
        let may_dir = MayConfig::may_dir().map_err(|_| SessionError::NoHomeDir)?;
        let home_paths = MayHomePaths::new(may_dir.clone());
        fs::create_dir_all(home_paths.sessions_dir())?;

        // Ensure bundle is extracted and symlinks created
        Bundle::ensure_extracted(&may_dir)?;

        // Create MEMORY.md if it doesn't exist
        let memory_path = home_paths.memory_file();
        if !memory_path.exists() {
            fs::write(&memory_path, "")?;
        }

        let store = SessionStore::new(home_paths.clone());

        // Open the database
        let db_path = home_paths.db_file();
        let db = ChatDb::open(&db_path).await?;

        // Ensure default profiles (daily, work, local) exist and set first as current
        let default_profile_id = db.ensure_default_profiles().await?;

        let manager = Self {
            may_dir,
            store,
            db,
            current: None,
            current_profile_id: Some(default_profile_id),
        };
        manager.reconcile_container_profile_sessions().await?;

        Ok(manager)
    }

    /// Get the May directory path.
    pub fn may_dir(&self) -> &Path {
        &self.may_dir
    }

    /// Get the database handle.
    pub fn db(&self) -> &ChatDb {
        &self.db
    }

    /// Create a new session in the current profile (lazy - not persisted until first message).
    pub fn create_session(&mut self) -> Result<&Session, SessionError> {
        self.create_session_with_model(None)
    }

    /// Create a new session with a specific model in the current profile.
    pub fn create_session_with_model(
        &mut self,
        model: Option<String>,
    ) -> Result<&Session, SessionError> {
        self.create_session_in_profile(model, self.current_profile_id.clone())
    }

    /// Create a new session with a specific model and profile.
    pub fn create_session_in_profile(
        &mut self,
        model: Option<String>,
        profile_id: Option<String>,
    ) -> Result<&Session, SessionError> {
        let id = SessionId::new();
        let now = Timestamp::now();

        // Don't create directories yet - defer until first message
        let sandbox_dir = self.store.sandbox_dir(id.as_str(), profile_id.as_deref());
        let session = Session {
            id,
            title: None,
            model,
            profile_id,
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
        let (session_id, title, model, profile_id, already_persisted) = {
            let session = self.current.as_ref().ok_or(SessionError::NoActiveSession)?;
            (
                session.id.as_str().to_string(),
                session.title.clone(),
                session.model.clone(),
                session.profile_id.clone(),
                session.persisted,
            )
        };

        if !already_persisted {
            let layout = self.session_layout(profile_id.as_deref()).await?;
            // Create filesystem directories (for sandbox, images, etc.)
            self.store
                .create_session_dirs(&session_id, profile_id.as_deref(), layout)
                .await?;

            // Create session in database with profile_id
            self.db
                .create_session_in_profile(
                    &session_id,
                    title.as_deref(),
                    model.as_deref(),
                    profile_id.as_deref(),
                )
                .await?;

            if let Some(session) = self.current.as_mut()
                && session.id.as_str() == session_id
            {
                session.persisted = true;
            }
        }
        Ok(())
    }

    /// Load an existing session by ID (from database).
    pub async fn load_session(&mut self, id: &SessionId) -> Result<&Session, SessionError> {
        let db_session = self
            .db
            .get_session(id.as_str())
            .await?
            .ok_or_else(|| SessionError::NotFound(id.as_str().to_string()))?;

        let title = db_session.title.clone();
        let model = db_session.model.clone();
        let profile_id = db_session.profile_id.clone();
        if self.session_layout(profile_id.as_deref()).await?
            == SandboxResourceLayout::ContainerMountPoints
        {
            self.store
                .reconcile_container_mount_points(id.as_str(), profile_id.as_deref())
                .await?;
        }
        let created_at = Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", &db_session.created_at)
            .unwrap_or_else(|_| Timestamp::now());
        let updated_at = Timestamp::strptime("%Y-%m-%dT%H:%M:%S%.fZ", &db_session.updated_at)
            .unwrap_or_else(|_| Timestamp::now());

        let history = self.load_history_from_db(id.as_str()).await?;

        let sandbox_dir = self.store.sandbox_dir(id.as_str(), profile_id.as_deref());
        let session = Session {
            id: id.clone(),
            title,
            model,
            profile_id,
            history,
            sandbox_dir,
            created_at,
            updated_at,
            persisted: true,
        };

        self.current = Some(session);
        Ok(self.current.as_ref().expect("just assigned"))
    }

    /// Force reconcile container mount-point layout for one persisted session.
    pub async fn reconcile_container_layout_for_session(
        &self,
        id: &SessionId,
    ) -> Result<(), SessionError> {
        let profile_id = if let Some(session) = &self.current {
            if session.id == *id {
                session.profile_id.clone()
            } else {
                self.db
                    .get_session(id.as_str())
                    .await?
                    .and_then(|session| session.profile_id)
            }
        } else {
            self.db
                .get_session(id.as_str())
                .await?
                .and_then(|session| session.profile_id)
        };
        self.store
            .reconcile_container_mount_points(id.as_str(), profile_id.as_deref())
            .await?;
        Ok(())
    }

    async fn session_layout(
        &self,
        profile_id: Option<&str>,
    ) -> Result<SandboxResourceLayout, SessionError> {
        let Some(profile_id) = profile_id else {
            return Ok(SandboxResourceLayout::Symlinked);
        };
        let profile = self.db.get_profile(profile_id).await?;
        let layout = if profile
            .as_ref()
            .map(|p| p.environment.as_str() == "container")
            .unwrap_or(false)
        {
            SandboxResourceLayout::ContainerMountPoints
        } else {
            SandboxResourceLayout::Symlinked
        };
        Ok(layout)
    }

    async fn reconcile_container_profile_sessions(&self) -> Result<(), SessionError> {
        let profiles = self.db.list_profiles().await?;
        let container_profiles = profiles
            .iter()
            .filter(|profile| profile.environment.as_str() == "container")
            .count();
        tracing::info!(
            container_profiles,
            "Reconciling container session mount points"
        );
        for profile in profiles
            .iter()
            .filter(|profile| profile.environment.as_str() == "container")
        {
            let sessions = self.db.list_sessions_for_profile(&profile.id).await?;
            tracing::info!(
                profile_id = %profile.id,
                session_count = sessions.len(),
                "Reconciling sessions for container profile"
            );
            for session in sessions {
                self.store
                    .reconcile_container_mount_points(&session.id, Some(profile.id.as_str()))
                    .await?;
            }
        }
        Ok(())
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
            let stats = map_db_message_stats(&msg);

            history.push(ChatMessage {
                role,
                content: msg.content,
                timestamp,
                tool_call_id: msg.tool_call_id,
                stats,
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
        let stats = message.stats.as_ref().map(map_chat_stats_to_db_params);

        // Write to database
        self.db
            .append_message(
                &session_id,
                message.role.clone(),
                &message.content,
                ts_string.as_deref(),
                message.tool_call_id.as_deref(),
                stats.as_ref(),
            )
            .await?;

        // Update in-memory history
        session.history.push(message);

        Ok(())
    }

    /// Attach usage/context stats to the latest assistant message.
    pub async fn set_latest_assistant_stats(
        &mut self,
        stats: ChatMessageStats,
    ) -> Result<(), SessionError> {
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        let session_id = session.id.as_str().to_string();

        let Some(last_assistant) = session
            .history
            .messages_mut()
            .iter_mut()
            .rev()
            .find(|msg| msg.role == MessageRole::Assistant)
        else {
            return Err(SessionError::NotFound(
                "no assistant message available for stats".to_string(),
            ));
        };
        last_assistant.stats = Some(stats.clone());

        self.db
            .set_latest_assistant_stats(&session_id, &map_chat_stats_to_db_params(&stats))
            .await?;
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
            .write_history(session.id.as_str(), session.profile_id.as_deref(), &session.history)
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
            .write_history(session.id.as_str(), session.profile_id.as_deref(), &session.history)
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
            .write_history(session.id.as_str(), session.profile_id.as_deref(), &session.history)
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

    /// Get the current profile ID.
    pub fn current_profile_id(&self) -> Option<&str> {
        self.current_profile_id.as_deref()
    }

    /// Switch to a different profile.
    pub fn set_current_profile(&mut self, profile_id: String) {
        self.current_profile_id = Some(profile_id);
    }

    /// Delete a session from both DB and filesystem.
    pub async fn delete_session(&mut self, id: &str) -> Result<(), SessionError> {
        let profile_id = self
            .db
            .get_session(id)
            .await?
            .and_then(|session| session.profile_id);
        self.db.delete_session(id).await?;
        self.store.delete_session(id, profile_id.as_deref()).await?;
        Ok(())
    }
}
