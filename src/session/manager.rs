//! Session management for May agent.

use async_fs;
use jiff::Timestamp;
use petname::{Generator, Petnames};
use std::path::{Path, PathBuf};

use super::store::SandboxResourceLayout;
use super::{ChatHistory, ChatMessage, ChatMessageStats, MessageRole, SessionStore};
use super::{MayHomePaths, SessionRuntimeSummary};
use super::{PersistedMessage, PersistedMessageError};
use crate::bundle::Bundle;
use crate::config::MayConfig;
use crate::db::{ChatDb, ChatDbError};

/// Session management error.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No usable home directory could be determined.
    #[error("Home directory not found")]
    NoHomeDir,
    /// Filesystem I/O failed.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// Session store operation failed.
    #[error("Store error: {0}")]
    Store(#[from] super::store::StoreError),
    /// Database operation failed.
    #[error("Database error: {0}")]
    Database(#[from] ChatDbError),
    /// The referenced session does not exist.
    #[error("Session not found: {0}")]
    NotFound(String),
    /// No session is currently active.
    #[error("No active session")]
    NoActiveSession,
    /// Embedded resource bundle extraction failed.
    #[error("Bundle error: {0}")]
    Bundle(#[from] crate::bundle::BundleError),
    /// Container runtime preparation failed.
    #[error("Container runtime preparation failed: {0}")]
    Container(#[from] crate::container::ContainerError),
    /// A persisted message failed to decode.
    #[error("Persisted message error: {0}")]
    PersistedMessage(#[from] PersistedMessageError),
}

/// Unique session identifier (four random words joined by `-`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, skyzen::ToSchema)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Create a new random session ID using four words.
    ///
    /// # Panics
    /// Panics if the embedded petname dictionary cannot produce an id.
    #[must_use]
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
    #[must_use]
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
        write!(f, "{}", self.0)
    }
}

/// A session representing a conversation with the agent.
#[derive(Debug)]
pub struct Session {
    /// Unique session identifier.
    pub id: SessionId,
    /// Title generated for the session, once the title task finishes.
    pub title: Option<String>,
    /// Model bound to this session (set via /model command).
    pub model: Option<String>,
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    /// In-memory chat history.
    pub history: ChatHistory,
    /// Directory where `TerminalTool` creates its working directory.
    pub sandbox_dir: PathBuf,
    /// Creation timestamp.
    pub created_at: Timestamp,
    /// Last update timestamp.
    pub updated_at: Timestamp,
    /// Whether this session has been persisted to disk.
    pub persisted: bool,
}

/// Summary of a session for listing.
#[derive(Debug, Clone, serde::Serialize, skyzen::ToSchema)]
pub struct SessionSummary {
    /// Unique session identifier.
    pub id: SessionId,
    /// Session title, once assigned.
    pub title: Option<String>,
    /// Model bound to the session.
    pub model: Option<String>,
    /// Profile the session belongs to.
    pub profile_id: Option<String>,
    /// Whether the session is pinned in its profile.
    pub pinned: bool,
    /// Whether the session has been auto-archived.
    pub archived: bool,
    /// Folder the session is grouped in.
    pub folder_id: Option<String>,
    /// Display order within the pinned section.
    pub pin_sort_order: Option<i32>,
    /// Number of persisted messages.
    pub message_count: usize,
    /// Creation timestamp.
    #[schema(value_type = Option<String>)]
    pub created_at: Option<Timestamp>,
    /// Last update timestamp.
    #[schema(value_type = Option<String>)]
    pub updated_at: Option<Timestamp>,
    /// Content preview for searching (concatenated messages).
    pub content_preview: String,
    /// Optional per-session reasoning/thinking override ("low"|"medium"|"high").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Persisted runtime summary for refresh-safe UI state.
    #[serde(
        default,
        skip_serializing_if = "crate::session::SessionRuntimeSummary::is_empty"
    )]
    pub runtime: SessionRuntimeSummary,
}

/// Where a prompt-level rewind truncated the conversation.
#[derive(Debug, Clone, Copy)]
pub struct RewindPoint {
    /// Position in [`ChatHistory::messages`] where truncation began; the
    /// prompt's hidden-context rows precede the user message and are
    /// included in the cut.
    pub cut_index: usize,
    /// `messages` row id of the removed user message; it doubles as the
    /// file-tracker prompt checkpoint key.
    pub user_message_id: i64,
}

impl SessionSummary {
    /// Convert a DB session model to a summary.
    ///
    /// # Panics
    ///
    /// Panics when the row's `message_count` is negative — database rows are
    /// expected to satisfy that invariant.
    #[must_use]
    pub fn from_db_model(s: crate::db::entity::sessions::Model) -> Self {
        let created_at = s.created_at.parse::<Timestamp>().ok();
        let updated_at = s.updated_at.parse::<Timestamp>().ok();

        Self {
            id: SessionId::from_string(s.id),
            title: s.title,
            model: s.model,
            profile_id: s.profile_id,
            pinned: s.pinned,
            archived: s.archived,
            folder_id: s.folder_id,
            pin_sort_order: s.pin_sort_order,
            message_count: usize::try_from(s.message_count)
                .expect("session row message_count must be non-negative"),
            created_at,
            updated_at,
            content_preview: s.preview.unwrap_or_default(),
            reasoning_effort: s.reasoning_effort,
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

/// Load the persisted chat history for a session from the database.
///
/// # Errors
/// Returns [`SessionError`] on storage failure or malformed rows.
pub async fn load_history_from_db(
    db: &ChatDb,
    session_id: &str,
) -> Result<ChatHistory, SessionError> {
    let db_session = db.get_session(session_id).await?;
    let db_messages = db.get_messages(session_id).await?;

    let mut history = ChatHistory::new();

    if let Some(session) = db_session {
        history.metadata.title = session.title;
        history.metadata.model = session.model;
    }

    for msg in db_messages {
        let persisted = PersistedMessage::try_from(&msg)?;
        let role = persisted.role.clone();
        let timestamp = persisted.timestamp;
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

/// List all session summaries from a [`ChatDb`] handle.
///
/// This is a standalone function that doesn't require a [`SessionManager`] lock,
/// useful for spawned tasks that only need a `ChatDb` clone.
///
/// # Errors
///
/// Returns [`SessionError`] when the session list query or summarization
/// fails.
pub async fn list_summaries_from_db(db: &ChatDb) -> Result<Vec<SessionSummary>, SessionError> {
    let db_sessions = db.list_sessions().await?;
    Ok(summarize_db_sessions(db, db_sessions).await?)
}

/// Summarize DB session rows into [`SessionSummary`] values with runtime state.
///
/// # Errors
/// Returns [`ChatDbError`] on storage failure.
pub async fn summarize_db_sessions(
    db: &ChatDb,
    db_sessions: Vec<crate::db::entity::sessions::Model>,
) -> Result<Vec<SessionSummary>, ChatDbError> {
    let session_ids = db_sessions
        .iter()
        .map(|session| session.id.clone())
        .collect::<Vec<_>>();
    let mut runtime_by_session = db.list_session_runtime_summaries(&session_ids).await?;
    let mut summaries = Vec::with_capacity(db_sessions.len());
    for session in db_sessions {
        let mut summary = SessionSummary::from_db_model(session);
        summary.runtime = runtime_by_session
            .remove(summary.id.as_str())
            .unwrap_or_default();
        summaries.push(summary);
    }
    Ok(summaries)
}

/// Initialize on-disk session storage and the database under `may_dir`, returning the store, db, and default profile id.
///
/// # Errors
/// Returns [`SessionError`] when directories cannot be created or the database/schema setup fails.
pub async fn initialize_session_persistence(
    may_dir: &Path,
) -> Result<(SessionStore, ChatDb, String), SessionError> {
    let home_paths = MayHomePaths::new(may_dir.to_path_buf());
    async_fs::create_dir_all(home_paths.sessions_dir()).await?;

    Bundle::ensure_extracted(may_dir).await?;

    let memory_path = home_paths.memory_file();
    if async_fs::metadata(&memory_path).await.is_err() {
        async_fs::write(&memory_path, "").await?;
    }

    let store = SessionStore::new(home_paths.clone());
    let db = ChatDb::open(&home_paths.db_file()).await?;
    let default_profile_id = db.ensure_default_profiles().await?;
    for profile in db.list_profiles().await? {
        store.sync_profile_scoped_resources(&profile.id).await?;
    }
    reconcile_container_profile_sessions(&store, &db).await?;
    Ok((store, db, default_profile_id))
}

/// Reconcile container mount-point layout for sessions whose profiles run in containers.
///
/// # Errors
/// Returns [`SessionError`] on storage failure or when a session's layout cannot be reconciled.
pub async fn reconcile_container_profile_sessions(
    store: &SessionStore,
    db: &ChatDb,
) -> Result<(), SessionError> {
    let profiles = db.list_profiles().await?;
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
        let sessions = db.list_sessions_for_profile(&profile.id).await?;
        tracing::info!(
            profile_id = %profile.id,
            session_count = sessions.len(),
            "Reconciling sessions for container profile"
        );
        for session in sessions {
            store
                .reconcile_container_mount_points(&session.id, Some(profile.id.as_str()))
                .await?;
        }
    }
    Ok(())
}

/// Manages sessions and their lifecycle.
#[derive(Debug)]
pub struct SessionManager {
    may_dir: PathBuf,
    store: SessionStore,
    db: ChatDb,
    current: Option<Session>,
    /// The currently active profile ID (sessions are created within this profile).
    current_profile_id: Option<String>,
}

impl SessionManager {
    /// Create a new `SessionManager`, initializing ~/.may/ structure if needed.
    ///
    /// # Errors
    /// Returns [`SessionError::NoHomeDir`] when no home directory exists, or [`SessionError`] when storage setup fails.
    pub async fn new() -> Result<Self, SessionError> {
        let may_dir = MayConfig::may_dir().map_err(|_| SessionError::NoHomeDir)?;
        let (store, db, default_profile_id) = initialize_session_persistence(&may_dir).await?;

        let manager = Self {
            may_dir,
            store,
            db,
            current: None,
            current_profile_id: Some(default_profile_id),
        };
        Ok(manager)
    }

    /// Get the May directory path.
    #[must_use]
    pub fn may_dir(&self) -> &Path {
        &self.may_dir
    }

    /// Get the database handle.
    #[must_use]
    pub const fn db(&self) -> &ChatDb {
        &self.db
    }

    /// Create a new session in the current profile (lazy - not persisted until first message).
    ///
    /// # Errors
    /// Returns [`SessionError`] when the session cannot be created.
    pub fn create_session(&mut self) -> Result<&Session, SessionError> {
        self.create_session_with_model(None)
    }

    /// Create a new session with a specific model in the current profile.
    ///
    /// # Errors
    /// Returns [`SessionError`] when the session cannot be created.
    pub fn create_session_with_model(
        &mut self,
        model: Option<String>,
    ) -> Result<&Session, SessionError> {
        self.create_session_in_profile(model, self.current_profile_id.clone())
    }

    /// Create a new session with a specific model and profile.
    ///
    /// # Errors
    /// Returns [`SessionError`] when the session cannot be created.
    ///
    /// # Panics
    /// Panics if the freshly created session cannot be retrieved (internal invariant).
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
    ///
    /// # Errors
    /// Returns [`SessionError::NoActiveSession`] when no session is active, or [`SessionError`] on persistence failure.
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
            if layout == SandboxResourceLayout::ContainerMountPoints {
                crate::container::profile::ensure_container_runtime_assets(&self.may_dir).await?;
            }
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
    ///
    /// # Errors
    /// Returns [`SessionError::NotFound`] when the session does not exist, or [`SessionError`] on storage/layout failure.
    ///
    /// # Panics
    /// Panics if the freshly loaded session is missing from `self.current`,
    /// which cannot happen given the immediately preceding assignment.
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
        let created_at = db_session
            .created_at
            .parse::<Timestamp>()
            .unwrap_or_else(|_| Timestamp::now());
        let updated_at = db_session
            .updated_at
            .parse::<Timestamp>()
            .unwrap_or_else(|_| Timestamp::now());

        let history = load_history_from_db(&self.db, id.as_str()).await?;

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
    ///
    /// # Errors
    /// Returns [`SessionError`] on storage failure or when the layout cannot be reconciled.
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
            .is_some_and(|p| p.environment.as_str() == "container")
        {
            SandboxResourceLayout::ContainerMountPoints
        } else {
            SandboxResourceLayout::Symlinked
        };
        Ok(layout)
    }

    /// Append a message to the current session (DB + in-memory).
    ///
    /// Returns the persisted `messages` row id, which is stable for the
    /// session's lifetime and usable as a history anchor.
    ///
    /// # Errors
    /// Returns [`SessionError::NoActiveSession`] when no session is active, or [`SessionError`] on persistence failure.
    pub async fn append_message(&mut self, message: ChatMessage) -> Result<i64, SessionError> {
        self.ensure_persisted().await?;

        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        let session_id = session.id.as_str().to_string();

        let row = self.db.append_chat_message(&session_id, &message).await?;

        // Update in-memory history
        session.history.push(message);

        Ok(i64::from(row.id))
    }

    /// Rewind the current session's conversation to just before the user
    /// message at `prompt_index` (a position in
    /// [`ChatHistory::messages`]). The user message, its attached hidden
    /// context, and everything after it are deleted from the database and
    /// from the in-memory history.
    ///
    /// Returns the truncation point and the deleted user message's row id;
    /// the id doubles as the file-tracker prompt checkpoint key.
    ///
    /// # Errors
    /// Returns [`SessionError::NoActiveSession`] when no session is active,
    /// [`SessionError::NotFound`] when `prompt_index` does not point at a
    /// user message or the persisted rows diverge from the in-memory
    /// history, or [`SessionError`] on persistence failure.
    pub async fn rewind_to_prompt(
        &mut self,
        prompt_index: usize,
    ) -> Result<RewindPoint, SessionError> {
        self.ensure_persisted().await?;

        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        let session_id = session.id.as_str().to_string();

        if session
            .history
            .messages()
            .get(prompt_index)
            .is_none_or(|m| m.role != MessageRole::User)
        {
            return Err(SessionError::NotFound(format!(
                "history position {prompt_index} is not a user message"
            )));
        }

        // Hidden context rows injected for this prompt sit directly before
        // it; they belong to the abandoned timeline too.
        let mut cut = prompt_index;
        while cut > 0 && session.history.messages()[cut - 1].role == MessageRole::HiddenContext {
            cut -= 1;
        }

        let rows = self.db.get_messages(&session_id).await?;
        if rows.len() < session.history.len()
            || rows
                .get(prompt_index)
                .is_none_or(|row| row.role != MessageRole::User.as_str())
        {
            return Err(SessionError::NotFound(
                "persisted history diverged from the in-memory history".to_string(),
            ));
        }
        let user_message_id = i64::from(rows[prompt_index].id);
        let from_id = i64::from(rows[cut].id);

        self.db.delete_messages_from(&session_id, from_id).await?;
        session.history.messages_mut().truncate(cut);

        Ok(RewindPoint {
            cut_index: cut,
            user_message_id,
        })
    }

    /// Attach usage/context stats to the latest assistant message.
    ///
    /// # Errors
    /// Returns [`SessionError`] on persistence failure.
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
            .set_latest_assistant_chat_stats(&session_id, &stats)
            .await?;
        Ok(())
    }

    /// List all sessions (from database).
    ///
    /// # Errors
    /// Returns [`SessionError`] on storage failure.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, SessionError> {
        list_summaries_from_db(&self.db).await
    }

    /// Save current session state.
    ///
    /// # Errors
    /// Returns [`SessionError::NoActiveSession`] when no session is active, or [`SessionError`] on persistence failure.
    pub async fn save_current(&mut self) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        Ok(())
    }

    /// Set title for the current session.
    ///
    /// # Errors
    /// Returns [`SessionError::NoActiveSession`] when no session is active, or [`SessionError`] on persistence failure.
    pub async fn set_current_title(&mut self, title: String) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        session.title = Some(title.clone());
        session.history.metadata.title = Some(title.clone());

        self.db
            .update_session(session.id.as_str(), Some(&title), None, None)
            .await?;

        Ok(())
    }

    /// Set model for the current session.
    ///
    /// # Errors
    /// Returns [`SessionError::NoActiveSession`] when no session is active, or [`SessionError`] on persistence failure.
    pub async fn set_current_model(&mut self, model: String) -> Result<(), SessionError> {
        self.ensure_persisted().await?;
        let session = self.current.as_mut().ok_or(SessionError::NoActiveSession)?;
        session.model = Some(model.clone());
        session.history.metadata.model = Some(model.clone());

        self.db
            .update_session(session.id.as_str(), None, Some(&model), None)
            .await?;

        Ok(())
    }

    /// Get current session.
    #[must_use]
    pub const fn current(&self) -> Option<&Session> {
        self.current.as_ref()
    }

    /// Get current session mutably.
    pub const fn current_mut(&mut self) -> Option<&mut Session> {
        self.current.as_mut()
    }

    /// Get the store for direct access.
    #[must_use]
    pub const fn store(&self) -> &SessionStore {
        &self.store
    }

    /// Get the current profile ID.
    #[must_use]
    pub fn current_profile_id(&self) -> Option<&str> {
        self.current_profile_id.as_deref()
    }

    /// Switch to a different profile.
    pub fn set_current_profile(&mut self, profile_id: String) {
        self.current_profile_id = Some(profile_id);
    }

    /// Delete a session from both DB and filesystem.
    ///
    /// # Errors
    /// Returns [`SessionError::NotFound`] when the session does not exist, or [`SessionError`] on storage failure.
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
