//! Chat history database backed by SQLite via SeaORM.
//!
//! Single global database at `~/.may/history.db` containing all sessions and messages.

pub mod entity;

use std::path::Path;

use sea_orm::ConnectionTrait;
use sea_orm::{
    ActiveModelTrait,
    ActiveValue::NotSet,
    ColumnTrait, ConnectOptions, Database, DatabaseConnection, DbErr, EntityTrait, Order,
    PaginatorTrait, QueryFilter, QueryOrder, Schema, Set,
    sea_query::{Expr, Query, SqliteQueryBuilder},
};

use crate::session::{
    PendingUserMessage, PersistedAskUserRequest, PersistedPermissionRequest, SessionRuntimeState,
    SessionRuntimeSummary,
};
use entity::{
    context_debug_snapshots, folders, messages, profile_memories, profiles, push_subscriptions,
    scheduled_tasks, session_context_snapshots, session_pending_ask_user,
    session_pending_messages, session_pending_permissions, session_runtimes, sessions,
};

/// Error type for database operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatDbError {
    #[error("Database error: {0}")]
    Db(#[from] DbErr),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid profile environment: {0}")]
    InvalidProfileEnvironment(String),
    #[error(
        "Pinned limit exceeded for profile {profile_id}: current pinned items {current}, limit {limit}"
    )]
    PinnedLimitExceeded {
        profile_id: String,
        limit: i64,
        current: i64,
    },
}

/// Parameters for creating a new profile.
pub struct CreateProfileParams<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub environment: &'a str,
    pub container_image: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub persona: Option<&'a str>,
    pub preferences_json: Option<&'a str>,
    pub location_aware: bool,
}

/// Parameters for updating a profile's mutable fields.
pub struct UpdateProfileParams<'a> {
    pub name: Option<&'a str>,
    pub icon: Option<Option<&'a str>>,
    pub environment: Option<&'a str>,
    pub container_image: Option<Option<&'a str>>,
    pub container_id: Option<Option<&'a str>>,
    pub sort_order: Option<i32>,
    pub persona: Option<Option<&'a str>>,
    pub preferences_json: Option<Option<&'a str>>,
    pub location_aware: Option<bool>,
}

/// Persisted assistant usage/context stats.
#[derive(Debug, Clone, Default)]
pub struct MessageStatsParams {
    pub provider_id: Option<String>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub elapsed_ms: Option<u64>,
}

struct DefaultProfile<'a> {
    slug: &'static str,
    name: &'a str,
    icon: &'static str,
    environment: &'static str,
    location_aware: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileEnvironmentKind {
    Local,
    Container,
}

fn parse_profile_environment(value: &str) -> Result<ProfileEnvironmentKind, ChatDbError> {
    match value {
        "local" => Ok(ProfileEnvironmentKind::Local),
        "container" => Ok(ProfileEnvironmentKind::Container),
        other => Err(ChatDbError::InvalidProfileEnvironment(other.to_string())),
    }
}

/// Chat history database.
///
/// Wraps a SeaORM `DatabaseConnection` to a SQLite file.
#[derive(Debug, Clone)]
pub struct ChatDb {
    conn: DatabaseConnection,
}

impl ChatDb {
    /// Open (or create) the chat history database at the given path.
    ///
    /// Creates the schema if the tables don't exist. Applies migrations for new columns.
    pub async fn open(db_path: &Path) -> Result<Self, ChatDbError> {
        let url = format!("sqlite://{}?mode=rwc", db_path.display());
        let mut opts = ConnectOptions::new(url);
        opts.sqlx_logging(false);

        let conn = Database::connect(opts).await?;

        // Create tables if they don't exist
        let schema = Schema::new(sea_orm::DatabaseBackend::Sqlite);

        let sessions_stmt = schema.create_table_from_entity(sessions::Entity);
        let create_sql = sessions_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let messages_stmt = schema.create_table_from_entity(messages::Entity);
        let create_sql = messages_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Profiles and folders tables
        let profiles_stmt = schema.create_table_from_entity(profiles::Entity);
        let create_sql = profiles_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let folders_stmt = schema.create_table_from_entity(folders::Entity);
        let create_sql = folders_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Scheduled tasks table
        let tasks_stmt = schema.create_table_from_entity(scheduled_tasks::Entity);
        let create_sql = tasks_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Push subscriptions table
        let push_stmt = schema.create_table_from_entity(push_subscriptions::Entity);
        let create_sql = push_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Profile memories table
        let mem_stmt = schema.create_table_from_entity(profile_memories::Entity);
        let create_sql = mem_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Context debug snapshots table
        let context_debug_stmt = schema.create_table_from_entity(context_debug_snapshots::Entity);
        let create_sql = context_debug_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let session_context_stmt = schema.create_table_from_entity(session_context_snapshots::Entity);
        let create_sql = session_context_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let session_runtime_stmt = schema.create_table_from_entity(session_runtimes::Entity);
        let create_sql = session_runtime_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let pending_messages_stmt =
            schema.create_table_from_entity(session_pending_messages::Entity);
        let create_sql = pending_messages_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let pending_permissions_stmt =
            schema.create_table_from_entity(session_pending_permissions::Entity);
        let create_sql = pending_permissions_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let pending_ask_user_stmt =
            schema.create_table_from_entity(session_pending_ask_user::Entity);
        let create_sql = pending_ask_user_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Create indexes
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sessions_profile_id ON sessions(profile_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_sessions_archived ON sessions(archived, updated_at)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_folders_profile_id ON folders(profile_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_scheduled_tasks_profile ON scheduled_tasks(profile_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_profile_memories_profile ON profile_memories(profile_id, memory_type)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_context_debug_snapshots_timestamp ON context_debug_snapshots(timestamp)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_session_context_snapshots_updated_at ON session_context_snapshots(updated_at)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_session_pending_messages_session_created ON session_pending_messages(session_id, created_at)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_session_pending_permissions_session ON session_pending_permissions(session_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_session_pending_ask_user_session ON session_pending_ask_user(session_id)",
        )
        .await?;

        // Migrations: add new profile columns if missing
        Self::migrate_add_column(&conn, "profiles", "slug", "TEXT").await?;
        Self::migrate_add_column(&conn, "profiles", "persona", "TEXT").await?;
        Self::migrate_add_column(&conn, "profiles", "preferences_json", "TEXT").await?;
        Self::migrate_add_column(
            &conn,
            "profiles",
            "location_aware",
            "BOOLEAN NOT NULL DEFAULT 0",
        )
        .await?;
        Self::migrate_add_column(
            &conn,
            "scheduled_tasks",
            "trigger_kind",
            "TEXT NOT NULL DEFAULT 'time'",
        )
        .await?;
        Self::migrate_add_column(&conn, "messages", "provider_id", "TEXT").await?;
        Self::migrate_add_column(&conn, "messages", "prompt_tokens", "INTEGER").await?;
        Self::migrate_add_column(&conn, "messages", "completion_tokens", "INTEGER").await?;
        Self::migrate_add_column(&conn, "messages", "total_tokens", "INTEGER").await?;
        Self::migrate_add_column(&conn, "messages", "context_window", "INTEGER").await?;
        Self::migrate_add_column(&conn, "messages", "elapsed_ms", "INTEGER").await?;
        Self::migrate_default_profile_slugs(&conn).await?;

        // Enable WAL mode for better concurrent read/write performance
        conn.execute_unprepared("PRAGMA journal_mode=WAL").await?;

        Ok(Self { conn })
    }

    /// Get the underlying database connection.
    pub fn connection(&self) -> &DatabaseConnection {
        &self.conn
    }

    /// Add a column to a table if it doesn't already exist (SQLite migration helper).
    async fn migrate_add_column(
        conn: &DatabaseConnection,
        table: &str,
        column: &str,
        col_type: &str,
    ) -> Result<(), ChatDbError> {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {col_type}");
        match conn.execute_unprepared(&sql).await {
            Ok(_) => Ok(()),
            Err(DbErr::Exec(ref runtime_err))
                if runtime_err.to_string().contains("duplicate column") =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn migrate_default_profile_slugs(conn: &DatabaseConnection) -> Result<(), ChatDbError> {
        for (slug, name, environment) in [
            ("daily", "daily", "container"),
            ("work", "work", "container"),
            ("local", "local", "local"),
        ] {
            let sql = format!(
                "UPDATE profiles \
                 SET slug = '{slug}' \
                 WHERE id = (\
                   SELECT id FROM profiles \
                   WHERE slug IS NULL \
                     AND lower(name) = '{name}' \
                     AND environment = '{environment}' \
                   ORDER BY created_at ASC \
                   LIMIT 1\
                 )"
            );
            conn.execute_unprepared(&sql).await?;
        }
        Ok(())
    }

    // ── Session CRUD ─────────────────────────────────────────────

    /// Create a new session.
    pub async fn create_session(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<sessions::Model, ChatDbError> {
        self.create_session_in_profile(id, title, model, None).await
    }

    /// Create a new session within a specific profile.
    pub async fn create_session_in_profile(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
        profile_id: Option<&str>,
    ) -> Result<sessions::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active = sessions::ActiveModel {
            id: Set(id.to_string()),
            title: Set(title.map(String::from)),
            model: Set(model.map(String::from)),
            preview: Set(None),
            message_count: Set(0),
            profile_id: Set(profile_id.map(String::from)),
            pinned: Set(false),
            archived: Set(false),
            folder_id: Set(None),
            pin_sort_order: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = active.insert(&self.conn).await?;
        Ok(model)
    }

    /// Get a session by ID.
    pub async fn get_session(&self, id: &str) -> Result<Option<sessions::Model>, ChatDbError> {
        let session = sessions::Entity::find_by_id(id).one(&self.conn).await?;
        Ok(session)
    }

    /// Get the persisted runtime summary for a session.
    pub async fn get_session_runtime(
        &self,
        id: &str,
    ) -> Result<Option<SessionRuntimeSummary>, ChatDbError> {
        let session = self.get_session(id).await?;
        if session.is_none() {
            return Ok(None);
        }
        Ok(Some(self.load_session_runtime_summary(id).await?))
    }

    async fn load_session_runtime_summary(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let runtime_row = session_runtimes::Entity::find_by_id(session_id)
            .one(&self.conn)
            .await?;
        let message_rows = session_pending_messages::Entity::find()
            .filter(session_pending_messages::Column::SessionId.eq(session_id))
            .order_by(session_pending_messages::Column::CreatedAt, Order::Asc)
            .all(&self.conn)
            .await?;
        let permission_row = session_pending_permissions::Entity::find()
            .filter(session_pending_permissions::Column::SessionId.eq(session_id))
            .one(&self.conn)
            .await?;
        let ask_user_row = session_pending_ask_user::Entity::find()
            .filter(session_pending_ask_user::Column::SessionId.eq(session_id))
            .one(&self.conn)
            .await?;

        let mut pending_immediate = None;
        let mut queued_messages = Vec::new();
        for row in message_rows {
            let message = PendingUserMessage {
                content: row.content,
                attachments: serde_json::from_str(&row.attachments_json)?,
                hidden_context_xml: row.hidden_context_xml,
                created_at: row.created_at,
                delivery_mode: row.delivery_mode.parse().unwrap_or_default(),
            };
            if message.delivery_mode == crate::session::PendingUserMessageMode::Steer
                && pending_immediate.is_none()
            {
                pending_immediate = Some(message);
            } else {
                queued_messages.push(message);
            }
        }

        let mut summary = SessionRuntimeSummary {
            state: runtime_row
                .as_ref()
                .map(|row| row.state.as_str())
                .unwrap_or(SessionRuntimeState::Idle.as_str())
                .parse()
                .unwrap_or(SessionRuntimeState::Idle),
            pending_immediate,
            queued_messages,
            waiting_permission: permission_row.map(|row| PersistedPermissionRequest {
                id: row.request_id,
                request: crate::session::PersistedPermissionRequestPayload {
                    mode: row.mode,
                    script: row.script,
                    session_id: Some(row.session_id),
                },
            }),
            waiting_ask_user: ask_user_row
                .map(|row| -> Result<PersistedAskUserRequest, ChatDbError> {
                    Ok(PersistedAskUserRequest {
                        id: row.request_id,
                        questions: serde_json::from_str(&row.questions_json)?,
                    })
                })
                .transpose()?,
            updated_at: runtime_row.map(|row| row.updated_at),
            ..Default::default()
        };
        summary.normalize_state();
        Ok(summary)
    }

    async fn write_session_runtime_summary(
        &self,
        session_id: &str,
        summary: &SessionRuntimeSummary,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active_runtime = session_runtimes::ActiveModel {
            session_id: Set(session_id.to_string()),
            state: Set(summary.state.as_str().to_string()),
            updated_at: Set(now),
        };
        session_runtimes::Entity::insert(active_runtime)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(session_runtimes::Column::SessionId)
                    .update_columns([
                        session_runtimes::Column::State,
                        session_runtimes::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.conn)
            .await?;

        session_pending_messages::Entity::delete_many()
            .filter(session_pending_messages::Column::SessionId.eq(session_id))
            .exec(&self.conn)
            .await?;

        if let Some(message) = &summary.pending_immediate {
            session_pending_messages::ActiveModel {
                id: NotSet,
                session_id: Set(session_id.to_string()),
                delivery_mode: Set(message.delivery_mode.as_str().to_string()),
                content: Set(message.content.clone()),
                attachments_json: Set(serde_json::to_string(&message.attachments)?),
                hidden_context_xml: Set(message.hidden_context_xml.clone()),
                created_at: Set(message.created_at.clone()),
            }
            .insert(&self.conn)
            .await?;
        }
        for message in &summary.queued_messages {
            session_pending_messages::ActiveModel {
                id: NotSet,
                session_id: Set(session_id.to_string()),
                delivery_mode: Set(message.delivery_mode.as_str().to_string()),
                content: Set(message.content.clone()),
                attachments_json: Set(serde_json::to_string(&message.attachments)?),
                hidden_context_xml: Set(message.hidden_context_xml.clone()),
                created_at: Set(message.created_at.clone()),
            }
            .insert(&self.conn)
            .await?;
        }

        session_pending_permissions::Entity::delete_many()
            .filter(session_pending_permissions::Column::SessionId.eq(session_id))
            .exec(&self.conn)
            .await?;
        if let Some(request) = &summary.waiting_permission {
            session_pending_permissions::ActiveModel {
                request_id: Set(request.id.clone()),
                session_id: Set(session_id.to_string()),
                mode: Set(request.request.mode.clone()),
                script: Set(request.request.script.clone()),
                created_at: Set(summary
                    .updated_at
                    .clone()
                    .unwrap_or_else(|| jiff::Timestamp::now().to_string())),
            }
            .insert(&self.conn)
            .await?;
        }

        session_pending_ask_user::Entity::delete_many()
            .filter(session_pending_ask_user::Column::SessionId.eq(session_id))
            .exec(&self.conn)
            .await?;
        if let Some(request) = &summary.waiting_ask_user {
            session_pending_ask_user::ActiveModel {
                request_id: Set(request.id.clone()),
                session_id: Set(session_id.to_string()),
                questions_json: Set(serde_json::to_string(&request.questions)?),
                created_at: Set(summary
                    .updated_at
                    .clone()
                    .unwrap_or_else(|| jiff::Timestamp::now().to_string())),
            }
            .insert(&self.conn)
            .await?;
        }
        Ok(())
    }

    /// Set the base runtime state for a session, preserving persisted pending payloads.
    pub async fn set_session_runtime_state(
        &self,
        session_id: &str,
        state: SessionRuntimeState,
    ) -> Result<(), ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.state = state;
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await
    }

    /// Enqueue a pending-immediate message, replacing any previous pending-immediate payload.
    pub async fn set_pending_immediate_message(
        &self,
        session_id: &str,
        message: PendingUserMessage,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.pending_immediate = Some(message);
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Append a queued message to the server-owned queue.
    pub async fn push_queued_message(
        &self,
        session_id: &str,
        message: PendingUserMessage,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.queued_messages.push(message);
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Remove and return the next dispatchable pending message.
    pub async fn take_next_pending_message(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        let next = if summary.pending_immediate.is_some() {
            summary.pending_immediate.take()
        } else if !summary.queued_messages.is_empty() {
            Some(summary.queued_messages.remove(0))
        } else {
            None
        };
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(next)
    }

    /// Remove a persisted pending-immediate message, if present.
    pub async fn clear_pending_immediate_message(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.pending_immediate = None;
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Remove a queued message by its position.
    pub async fn remove_queued_message_at(
        &self,
        session_id: &str,
        index: usize,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        if index >= summary.queued_messages.len() {
            return Err(ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Queued message index {index} out of range for session {session_id}"
            ))));
        }
        summary.queued_messages.remove(index);
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Persist a waiting-permission request for refresh-safe recovery.
    pub async fn set_waiting_permission(
        &self,
        session_id: &str,
        request: Option<PersistedPermissionRequest>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.waiting_permission = request;
        if summary.waiting_permission.is_none() {
            summary.state = SessionRuntimeState::Running;
        }
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Persist a waiting ask-user request for refresh-safe recovery.
    pub async fn set_waiting_ask_user(
        &self,
        session_id: &str,
        request: Option<PersistedAskUserRequest>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.waiting_ask_user = request;
        if summary.waiting_ask_user.is_none() {
            summary.state = SessionRuntimeState::Running;
        }
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Check if a session exists in the database.
    pub async fn session_exists(&self, id: &str) -> Result<bool, ChatDbError> {
        Ok(sessions::Entity::find_by_id(id)
            .one(&self.conn)
            .await?
            .is_some())
    }

    /// Update session metadata (title and/or model).
    pub async fn update_session(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut active = sessions::ActiveModel {
            id: Set(id.to_string()),
            updated_at: Set(now),
            ..Default::default()
        };
        if let Some(t) = title {
            active.title = Set(Some(t.to_string()));
        }
        if let Some(m) = model {
            active.model = Set(Some(m.to_string()));
        }
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Delete a session and all its messages (CASCADE).
    pub async fn delete_session(&self, id: &str) -> Result<(), ChatDbError> {
        // Delete messages first (SQLite may not enforce FK CASCADE)
        messages::Entity::delete_many()
            .filter(messages::Column::SessionId.eq(id))
            .exec(&self.conn)
            .await?;
        session_context_snapshots::Entity::delete_by_id(id)
            .exec(&self.conn)
            .await?;
        sessions::Entity::delete_by_id(id).exec(&self.conn).await?;
        Ok(())
    }

    /// Delete all sessions.
    pub async fn delete_all_sessions(&self) -> Result<u64, ChatDbError> {
        let result = messages::Entity::delete_many().exec(&self.conn).await?;
        let snapshot_result = session_context_snapshots::Entity::delete_many()
            .exec(&self.conn)
            .await?;
        let session_result = sessions::Entity::delete_many().exec(&self.conn).await?;
        Ok(result.rows_affected + snapshot_result.rows_affected + session_result.rows_affected)
    }

    /// List all sessions ordered by updated_at descending.
    pub async fn list_sessions(&self) -> Result<Vec<sessions::Model>, ChatDbError> {
        let sessions = sessions::Entity::find()
            .order_by(sessions::Column::UpdatedAt, Order::Desc)
            .all(&self.conn)
            .await?;
        Ok(sessions)
    }

    /// Normalize persisted runtime state after a server restart.
    ///
    /// This only repairs durable state. Live resources such as streams,
    /// process handles, and SSH connections must be rebuilt lazily.
    pub async fn recover_session_runtimes_after_restart(&self) -> Result<(), ChatDbError> {
        let sessions = self.list_sessions().await?;
        for session in sessions {
            let session_id = session.id.clone();
            let mut summary = self.load_session_runtime_summary(&session_id).await?;
            let original_state = summary.state;

            summary.normalize_state();
            if matches!(original_state, SessionRuntimeState::Running)
                && summary.state == SessionRuntimeState::Running
            {
                summary.state = SessionRuntimeState::Idle;
            }

            if summary.state != original_state {
                self.write_session_runtime_summary(&session_id, &summary)
                    .await?;
            }
        }
        Ok(())
    }

    /// Update session stats: updated_at, preview, and optionally increment message_count.
    async fn update_session_stats(
        &self,
        session_id: &str,
        updated_at: &str,
        preview: &str,
        increment_count: bool,
    ) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::UpdatedAt, updated_at);
        update.value(sessions::Column::Preview, preview);

        if increment_count {
            update.value(
                sessions::Column::MessageCount,
                Expr::col(sessions::Column::MessageCount).add(1),
            );
        }

        update.and_where(Expr::col(sessions::Column::Id).eq(session_id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    // ── Message CRUD ─────────────────────────────────────────────

    /// Append a message to a session. Updates session preview and message count.
    pub async fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        timestamp: Option<&str>,
        tool_call_id: Option<&str>,
        stats: Option<&MessageStatsParams>,
    ) -> Result<messages::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let prompt_tokens = stats
            .and_then(|s| s.prompt_tokens)
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("prompt_tokens overflow: {e}")))?;
        let completion_tokens = stats
            .and_then(|s| s.completion_tokens)
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("completion_tokens overflow: {e}")))?;
        let total_tokens = stats
            .and_then(|s| s.total_tokens)
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("total_tokens overflow: {e}")))?;
        let context_window = stats
            .and_then(|s| s.context_window)
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("context_window overflow: {e}")))?;
        let elapsed_ms = stats
            .and_then(|s| s.elapsed_ms)
            .map(i64::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("elapsed_ms overflow: {e}")))?;
        let provider_id = stats.and_then(|s| s.provider_id.clone());

        // Insert the message
        let active = messages::ActiveModel {
            session_id: Set(session_id.to_string()),
            role: Set(role.to_string()),
            content: Set(content.to_string()),
            timestamp: Set(timestamp.map(String::from).or(Some(now.clone()))),
            tool_call_id: Set(tool_call_id.map(String::from)),
            provider_id: Set(provider_id),
            prompt_tokens: Set(prompt_tokens),
            completion_tokens: Set(completion_tokens),
            total_tokens: Set(total_tokens),
            context_window: Set(context_window),
            elapsed_ms: Set(elapsed_ms),
            ..Default::default()
        };
        let model = active.insert(&self.conn).await?;

        // Update session: touch updated_at, increment message_count, set preview
        let preview = truncate_preview(content);
        self.update_session_stats(session_id, &now, &preview, true)
            .await?;

        Ok(model)
    }

    /// Update usage/context stats on the latest assistant message in a session.
    pub async fn set_latest_assistant_stats(
        &self,
        session_id: &str,
        stats: &MessageStatsParams,
    ) -> Result<(), ChatDbError> {
        let Some(row) = messages::Entity::find()
            .filter(messages::Column::SessionId.eq(session_id))
            .filter(messages::Column::Role.eq("assistant"))
            .order_by_desc(messages::Column::Id)
            .one(&self.conn)
            .await?
        else {
            return Err(ChatDbError::Db(DbErr::RecordNotFound(format!(
                "no assistant message found in session {session_id}"
            ))));
        };

        let prompt_tokens = stats
            .prompt_tokens
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("prompt_tokens overflow: {e}")))?;
        let completion_tokens = stats
            .completion_tokens
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("completion_tokens overflow: {e}")))?;
        let total_tokens = stats
            .total_tokens
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("total_tokens overflow: {e}")))?;
        let context_window = stats
            .context_window
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("context_window overflow: {e}")))?;
        let elapsed_ms = stats
            .elapsed_ms
            .map(i64::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("elapsed_ms overflow: {e}")))?;

        let mut active: messages::ActiveModel = row.into();
        active.provider_id = Set(stats.provider_id.clone());
        active.prompt_tokens = Set(prompt_tokens);
        active.completion_tokens = Set(completion_tokens);
        active.total_tokens = Set(total_tokens);
        active.context_window = Set(context_window);
        active.elapsed_ms = Set(elapsed_ms);
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Get all messages for a session, ordered by id.
    pub async fn get_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<messages::Model>, ChatDbError> {
        let msgs = messages::Entity::find()
            .filter(messages::Column::SessionId.eq(session_id))
            .order_by_asc(messages::Column::Id)
            .all(&self.conn)
            .await?;
        Ok(msgs)
    }

    /// Get messages for a session filtered by role.
    pub async fn get_messages_by_role(
        &self,
        session_id: &str,
        role: &str,
    ) -> Result<Vec<messages::Model>, ChatDbError> {
        let msgs = messages::Entity::find()
            .filter(messages::Column::SessionId.eq(session_id))
            .filter(messages::Column::Role.eq(role))
            .order_by_asc(messages::Column::Id)
            .all(&self.conn)
            .await?;
        Ok(msgs)
    }

    // ── Context Debug Snapshot ────────────────────────────────

    /// Upsert the latest context-debug snapshot for a session.
    pub async fn upsert_context_debug_snapshot(
        &self,
        session_id: &str,
        timestamp: &str,
        model: &str,
        provider_id: &str,
        prompt: &str,
        prompt_attachments_json: &str,
        assembled_messages_json: &str,
    ) -> Result<(), ChatDbError> {
        if let Some(existing) = context_debug_snapshots::Entity::find_by_id(session_id)
            .one(&self.conn)
            .await?
        {
            let mut active: context_debug_snapshots::ActiveModel = existing.into();
            active.timestamp = Set(timestamp.to_string());
            active.model = Set(model.to_string());
            active.provider_id = Set(provider_id.to_string());
            active.prompt = Set(prompt.to_string());
            active.prompt_attachments_json = Set(prompt_attachments_json.to_string());
            active.assembled_messages_json = Set(assembled_messages_json.to_string());
            active.update(&self.conn).await?;
        } else {
            let active = context_debug_snapshots::ActiveModel {
                session_id: Set(session_id.to_string()),
                timestamp: Set(timestamp.to_string()),
                model: Set(model.to_string()),
                provider_id: Set(provider_id.to_string()),
                prompt: Set(prompt.to_string()),
                prompt_attachments_json: Set(prompt_attachments_json.to_string()),
                assembled_messages_json: Set(assembled_messages_json.to_string()),
            };
            active.insert(&self.conn).await?;
        }
        Ok(())
    }

    /// Load latest context-debug snapshot for a session.
    pub async fn get_context_debug_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<context_debug_snapshots::Model>, ChatDbError> {
        let row = context_debug_snapshots::Entity::find_by_id(session_id)
            .one(&self.conn)
            .await?;
        Ok(row)
    }

    /// Upsert the latest serialized agent context snapshot for a session.
    pub async fn upsert_session_context_snapshot(
        &self,
        session_id: &str,
        context_json: &str,
    ) -> Result<(), ChatDbError> {
        let updated_at = jiff::Timestamp::now().to_string();
        let active = session_context_snapshots::ActiveModel {
            session_id: Set(session_id.to_string()),
            context_json: Set(context_json.to_string()),
            updated_at: Set(updated_at),
        };
        session_context_snapshots::Entity::insert(active)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(session_context_snapshots::Column::SessionId)
                    .update_columns([
                        session_context_snapshots::Column::ContextJson,
                        session_context_snapshots::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    /// Load the serialized agent context snapshot for a session.
    pub async fn get_session_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<session_context_snapshots::Model>, ChatDbError> {
        session_context_snapshots::Entity::find_by_id(session_id)
            .one(&self.conn)
            .await
            .map_err(Into::into)
    }

    /// Delete a persisted context snapshot for a session.
    pub async fn delete_session_context_snapshot(
        &self,
        session_id: &str,
    ) -> Result<(), ChatDbError> {
        session_context_snapshots::Entity::delete_by_id(session_id)
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    // ── Permission / AskUser Queries ─────────────────────────────

    /// Check if network permission was ever approved in a session.
    pub async fn has_network_approval(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let responses = self
            .get_messages_by_role(session_id, "permission_response")
            .await?;

        let requests = self
            .get_messages_by_role(session_id, "permission_request")
            .await?;

        for resp in &responses {
            if let Ok(data) = serde_json::from_str::<PermissionResponsePayload>(&resp.content) {
                if !data.approved {
                    continue;
                }
                for req in &requests {
                    if let Ok(req_data) =
                        serde_json::from_str::<PermissionRequestPayload>(&req.content)
                        && req_data.id == data.id
                        && req_data.mode == "network"
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Find unanswered permission requests in a session.
    pub async fn pending_permission_requests(
        &self,
        session_id: &str,
    ) -> Result<Vec<PermissionRequestPayload>, ChatDbError> {
        let requests = self
            .get_messages_by_role(session_id, "permission_request")
            .await?;
        let responses = self
            .get_messages_by_role(session_id, "permission_response")
            .await?;

        let responded_ids: std::collections::HashSet<String> = responses
            .iter()
            .filter_map(|m| {
                serde_json::from_str::<PermissionResponsePayload>(&m.content)
                    .ok()
                    .map(|d| d.id)
            })
            .collect();

        let pending = requests
            .iter()
            .filter_map(|m| serde_json::from_str::<PermissionRequestPayload>(&m.content).ok())
            .filter(|d| !responded_ids.contains(&d.id))
            .collect();

        Ok(pending)
    }

    /// Find unanswered ask-user requests in a session.
    pub async fn pending_ask_user_requests(
        &self,
        session_id: &str,
    ) -> Result<Vec<AskUserRequestPayload>, ChatDbError> {
        let requests = self
            .get_messages_by_role(session_id, "ask_user_request")
            .await?;
        let responses = self
            .get_messages_by_role(session_id, "ask_user_response")
            .await?;

        let responded_ids: std::collections::HashSet<String> = responses
            .iter()
            .filter_map(|m| {
                serde_json::from_str::<AskUserResponsePayload>(&m.content)
                    .ok()
                    .map(|d| d.id)
            })
            .collect();

        let pending = requests
            .iter()
            .filter_map(|m| serde_json::from_str::<AskUserRequestPayload>(&m.content).ok())
            .filter(|d| !responded_ids.contains(&d.id))
            .collect();

        Ok(pending)
    }

    // ── Profile CRUD ────────────────────────────────────────────

    /// Create a new profile.
    pub async fn create_profile(
        &self,
        params: CreateProfileParams<'_>,
    ) -> Result<profiles::Model, ChatDbError> {
        let environment = parse_profile_environment(params.environment)?;
        let now = jiff::Timestamp::now().to_string();
        let max_order = self.max_profile_sort_order().await?;
        let active = profiles::ActiveModel {
            id: Set(params.id.to_string()),
            slug: Set(params.slug.map(String::from)),
            name: Set(params.name.to_string()),
            icon: Set(params.icon.map(String::from)),
            environment: Set(params.environment.to_string()),
            container_image: Set(match environment {
                ProfileEnvironmentKind::Local => None,
                ProfileEnvironmentKind::Container => params.container_image.map(String::from),
            }),
            container_id: Set(None),
            persona: Set(params.persona.map(String::from)),
            preferences_json: Set(params.preferences_json.map(String::from)),
            location_aware: Set(params.location_aware),
            pinned_limit: Set(10),
            sort_order: Set(max_order + 1),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = active.insert(&self.conn).await?;
        Ok(model)
    }

    /// List all profiles ordered by sort_order.
    pub async fn list_profiles(&self) -> Result<Vec<profiles::Model>, ChatDbError> {
        let rows = profiles::Entity::find()
            .order_by_asc(profiles::Column::SortOrder)
            .all(&self.conn)
            .await?;
        Ok(rows)
    }

    /// Get a profile by ID.
    pub async fn get_profile(&self, id: &str) -> Result<Option<profiles::Model>, ChatDbError> {
        let row = profiles::Entity::find_by_id(id).one(&self.conn).await?;
        Ok(row)
    }

    /// Update profile fields.
    pub async fn update_profile(
        &self,
        id: &str,
        params: UpdateProfileParams<'_>,
    ) -> Result<(), ChatDbError> {
        let next_environment = params
            .environment
            .map(parse_profile_environment)
            .transpose()?;
        let now = jiff::Timestamp::now().to_string();
        let mut active = profiles::ActiveModel {
            id: Set(id.to_string()),
            updated_at: Set(now),
            ..Default::default()
        };
        if let Some(n) = params.name {
            active.name = Set(n.to_string());
        }
        if let Some(ic) = params.icon {
            active.icon = Set(ic.map(String::from));
        }
        if let Some(environment) = params.environment {
            active.environment = Set(environment.to_string());
        }
        if let Some(container_image) = params.container_image {
            active.container_image = Set(container_image.map(String::from));
        }
        if matches!(next_environment, Some(ProfileEnvironmentKind::Local)) {
            active.container_image = Set(None);
            active.container_id = Set(None);
        }
        if let Some(cid) = params.container_id {
            active.container_id = Set(cid.map(String::from));
        }
        if let Some(sort_order) = params.sort_order {
            active.sort_order = Set(sort_order);
        }
        if let Some(p) = params.persona {
            active.persona = Set(p.map(String::from));
        }
        if let Some(pj) = params.preferences_json {
            active.preferences_json = Set(pj.map(String::from));
        }
        if let Some(la) = params.location_aware {
            active.location_aware = Set(la);
        }
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Delete a profile and reassign its sessions to the default profile.
    pub async fn delete_profile(&self, id: &str) -> Result<(), ChatDbError> {
        // Reassign sessions to no profile (NULL)
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::ProfileId, Option::<String>::None);
        update.and_where(Expr::col(sessions::Column::ProfileId).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;

        // Delete folders belonging to this profile
        folders::Entity::delete_many()
            .filter(folders::Column::ProfileId.eq(id))
            .exec(&self.conn)
            .await?;

        profiles::Entity::delete_by_id(id).exec(&self.conn).await?;
        Ok(())
    }

    /// Get a profile by its well-known slug ("daily", "work", "local").
    pub async fn get_profile_by_slug(
        &self,
        slug: &str,
    ) -> Result<Option<profiles::Model>, ChatDbError> {
        let row = profiles::Entity::find()
            .filter(profiles::Column::Slug.eq(slug))
            .one(&self.conn)
            .await?;
        Ok(row)
    }

    /// Ensure the three default profiles (daily, work, local) exist.
    ///
    /// Returns the ID of the first profile (daily).
    pub async fn ensure_default_profiles(&self) -> Result<String, ChatDbError> {
        let local_hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "local".to_string());

        let defaults = [
            DefaultProfile {
                slug: "daily",
                name: "Daily",
                icon: "\u{1F305}", // 🌅
                environment: "container",
                location_aware: true,
            },
            DefaultProfile {
                slug: "work",
                name: "Work",
                icon: "\u{1F4BC}", // 💼
                environment: "container",
                location_aware: false,
            },
            DefaultProfile {
                slug: "local",
                name: &local_hostname,
                icon: "\u{1F5A5}\u{FE0F}", // 🖥️
                environment: "local",
                location_aware: false,
            },
        ];

        let mut first_id = None;
        let mut daily_profile_id: Option<String> = None;

        for def in &defaults {
            let ensured = if let Some(existing) = self.get_profile_by_slug(def.slug).await? {
                self.apply_default_profile_correction(existing, def).await?
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                self.create_profile(CreateProfileParams {
                    id: &id,
                    name: def.name,
                    environment: def.environment,
                    container_image: None,
                    slug: Some(def.slug),
                    icon: Some(def.icon),
                    persona: None,
                    preferences_json: None,
                    location_aware: def.location_aware,
                })
                .await?
            };

            if first_id.is_none() {
                first_id = Some(ensured.id.clone());
            }
            if def.slug == "daily" {
                daily_profile_id = Some(ensured.id);
            }
        }

        if let Some(ref daily_id) = daily_profile_id {
            self.assign_legacy_sessions_to_daily(daily_id).await?;
        }

        // first_id is always set because we iterate at least one default profile
        Ok(first_id.expect("at least one default profile must be created"))
    }

    async fn apply_default_profile_correction(
        &self,
        existing: profiles::Model,
        def: &DefaultProfile<'_>,
    ) -> Result<profiles::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut active: profiles::ActiveModel = existing.into();
        active.slug = Set(Some(def.slug.to_string()));
        active.name = Set(def.name.to_string());
        active.icon = Set(Some(def.icon.to_string()));
        active.environment = Set(def.environment.to_string());
        active.location_aware = Set(def.location_aware);
        if def.environment == "local" {
            active.container_image = Set(None);
            active.container_id = Set(None);
        }
        active.updated_at = Set(now);
        let model = active.update(&self.conn).await?;
        Ok(model)
    }

    async fn assign_legacy_sessions_to_daily(
        &self,
        daily_profile_id: &str,
    ) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::ProfileId, daily_profile_id.to_string());
        update.and_where(Expr::col(sessions::Column::ProfileId).is_null());
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    async fn max_profile_sort_order(&self) -> Result<i32, ChatDbError> {
        let row = self
            .conn
            .query_one(sea_orm::Statement::from_string(
                self.conn.get_database_backend(),
                "SELECT COALESCE(MAX(sort_order), -1) as max_order FROM profiles".to_string(),
            ))
            .await?;
        let max_order = row
            .and_then(|r| r.try_get_by_index::<i32>(0).ok())
            .unwrap_or(-1);
        Ok(max_order)
    }

    // ── Folder CRUD ─────────────────────────────────────────────

    /// Create a folder in a profile.
    pub async fn create_folder(
        &self,
        id: &str,
        profile_id: &str,
        name: &str,
    ) -> Result<folders::Model, ChatDbError> {
        self.ensure_pinned_capacity(profile_id).await?;
        let now = jiff::Timestamp::now().to_string();
        let max_order = self.max_folder_sort_order(profile_id).await?;
        let active = folders::ActiveModel {
            id: Set(id.to_string()),
            profile_id: Set(profile_id.to_string()),
            name: Set(name.to_string()),
            sort_order: Set(max_order + 1),
            created_at: Set(now),
        };
        let model = active.insert(&self.conn).await?;
        Ok(model)
    }

    /// List folders for a profile.
    pub async fn list_folders(&self, profile_id: &str) -> Result<Vec<folders::Model>, ChatDbError> {
        let rows = folders::Entity::find()
            .filter(folders::Column::ProfileId.eq(profile_id))
            .order_by_asc(folders::Column::SortOrder)
            .all(&self.conn)
            .await?;
        Ok(rows)
    }

    /// Update a folder's name.
    pub async fn update_folder(&self, id: &str, name: &str) -> Result<(), ChatDbError> {
        let active = folders::ActiveModel {
            id: Set(id.to_string()),
            name: Set(name.to_string()),
            ..Default::default()
        };
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Update a folder's sort order.
    pub async fn update_folder_sort_order(
        &self,
        id: &str,
        sort_order: i32,
    ) -> Result<(), ChatDbError> {
        let active = folders::ActiveModel {
            id: Set(id.to_string()),
            sort_order: Set(sort_order),
            ..Default::default()
        };
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Delete a folder and unpin its sessions.
    pub async fn delete_folder(&self, id: &str) -> Result<(), ChatDbError> {
        // Remove folder_id from sessions in this folder and unpin them
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::FolderId, Option::<String>::None);
        update.value(sessions::Column::Pinned, false);
        update.value(sessions::Column::PinSortOrder, Option::<i32>::None);
        update.and_where(Expr::col(sessions::Column::FolderId).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;

        folders::Entity::delete_by_id(id).exec(&self.conn).await?;
        Ok(())
    }

    async fn max_folder_sort_order(&self, profile_id: &str) -> Result<i32, ChatDbError> {
        let row = self
            .conn
            .query_one(sea_orm::Statement::from_string(
                self.conn.get_database_backend(),
                format!(
                    "SELECT COALESCE(MAX(sort_order), -1) as max_order FROM folders WHERE profile_id = '{}'",
                    profile_id.replace('\'', "''")
                ),
            ))
            .await?;
        let max_order = row
            .and_then(|r| r.try_get_by_index::<i32>(0).ok())
            .unwrap_or(-1);
        Ok(max_order)
    }

    // ── Session Profile Operations ──────────────────────────────

    /// Pin a session in its profile.
    pub async fn pin_session(&self, id: &str) -> Result<(), ChatDbError> {
        let session = self.get_session(id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!("Session not found: {id}")))
        })?;

        let profile_id = session.profile_id.as_deref().ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session {id} is not attached to a profile"
            )))
        })?;

        // Adding a new standalone pinned session consumes one pinned slot.
        // Moving a folder-contained pinned session to top-level also consumes one slot.
        if !session.pinned || session.folder_id.is_some() {
            self.ensure_pinned_capacity(profile_id).await?;
        }

        let max_order = self.max_pin_sort_order(profile_id).await?;

        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::Pinned, true);
        update.value(sessions::Column::Archived, false);
        update.value(sessions::Column::PinSortOrder, max_order + 1);
        update.and_where(Expr::col(sessions::Column::Id).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    /// Unpin a session.
    pub async fn unpin_session(&self, id: &str) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::Pinned, false);
        update.value(sessions::Column::FolderId, Option::<String>::None);
        update.value(sessions::Column::PinSortOrder, Option::<i32>::None);
        update.and_where(Expr::col(sessions::Column::Id).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    /// Archive a session.
    pub async fn archive_session(&self, id: &str) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::Archived, true);
        // Archived sessions must always leave pinned structures.
        update.value(sessions::Column::Pinned, false);
        update.value(sessions::Column::FolderId, Option::<String>::None);
        update.value(sessions::Column::PinSortOrder, Option::<i32>::None);
        update.and_where(Expr::col(sessions::Column::Id).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    /// Unarchive (reopen) a session.
    pub async fn unarchive_session(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::Archived, false);
        // Re-opened sessions always return to the regular list.
        update.value(sessions::Column::Pinned, false);
        update.value(sessions::Column::FolderId, Option::<String>::None);
        update.value(sessions::Column::PinSortOrder, Option::<i32>::None);
        update.value(sessions::Column::UpdatedAt, now.as_str());
        update.and_where(Expr::col(sessions::Column::Id).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    /// Move a session to a folder (or out of a folder if `folder_id` is None).
    pub async fn move_session_to_folder(
        &self,
        session_id: &str,
        folder_id: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;

        if session.archived {
            return Err(ChatDbError::Db(DbErr::Custom(format!(
                "cannot move archived session {session_id} into pinned folders"
            ))));
        }

        if folder_id.is_some() && !session.pinned {
            return Err(ChatDbError::Db(DbErr::Custom(format!(
                "cannot move unpinned session {session_id} into a pinned folder"
            ))));
        }

        // Moving a session out of a folder (while pinned) creates a new standalone
        // pinned item and therefore consumes profile pin capacity.
        if folder_id.is_none() && session.pinned && session.folder_id.is_some() {
            let profile_id = session.profile_id.as_deref().ok_or_else(|| {
                ChatDbError::Db(DbErr::RecordNotFound(format!(
                    "Session {session_id} is not attached to a profile"
                )))
            })?;
            self.ensure_pinned_capacity(profile_id).await?;
        }

        if let Some(target_folder_id) = folder_id {
            let folder = folders::Entity::find_by_id(target_folder_id)
                .one(&self.conn)
                .await?
                .ok_or_else(|| {
                    ChatDbError::Db(DbErr::RecordNotFound(format!(
                        "Folder not found: {target_folder_id}"
                    )))
                })?;

            let session_profile = session.profile_id.as_deref().ok_or_else(|| {
                ChatDbError::Db(DbErr::RecordNotFound(format!(
                    "Session {session_id} is not attached to a profile"
                )))
            })?;
            if folder.profile_id != session_profile {
                return Err(ChatDbError::Db(DbErr::RecordNotFound(format!(
                    "Folder {target_folder_id} does not belong to session profile {session_profile}"
                ))));
            }
        }

        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::FolderId, folder_id.map(String::from));
        update.and_where(Expr::col(sessions::Column::Id).eq(session_id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    /// Set a session's profile_id.
    pub async fn set_session_profile(
        &self,
        session_id: &str,
        profile_id: &str,
    ) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::ProfileId, profile_id);
        update.and_where(Expr::col(sessions::Column::Id).eq(session_id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
        Ok(())
    }

    /// List sessions for a specific profile, ordered by updated_at descending.
    pub async fn list_sessions_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<sessions::Model>, ChatDbError> {
        let rows = sessions::Entity::find()
            .filter(sessions::Column::ProfileId.eq(profile_id))
            .order_by(sessions::Column::UpdatedAt, Order::Desc)
            .all(&self.conn)
            .await?;
        Ok(rows)
    }

    /// Find non-pinned, non-archived sessions with `updated_at` before the cutoff.
    pub async fn sessions_needing_archive(
        &self,
        cutoff: &str,
    ) -> Result<Vec<sessions::Model>, ChatDbError> {
        let active_session_ids = session_runtimes::Entity::find()
            .filter(session_runtimes::Column::State.ne(SessionRuntimeState::Idle.as_str()))
            .all(&self.conn)
            .await?
            .into_iter()
            .map(|row| row.session_id)
            .collect::<Vec<_>>();

        let mut query = sessions::Entity::find()
            .filter(sessions::Column::Pinned.eq(false))
            .filter(sessions::Column::Archived.eq(false))
            .filter(sessions::Column::UpdatedAt.lt(cutoff));
        if !active_session_ids.is_empty() {
            query = query.filter(sessions::Column::Id.is_not_in(active_session_ids));
        }
        let rows = query.all(&self.conn).await?;
        Ok(rows)
    }

    /// Count pinned items (sessions + folders) in a profile.
    pub async fn pinned_count(&self, profile_id: &str) -> Result<i64, ChatDbError> {
        let session_count: i64 = sessions::Entity::find()
            .filter(sessions::Column::ProfileId.eq(profile_id))
            .filter(sessions::Column::Pinned.eq(true))
            .filter(sessions::Column::FolderId.is_null())
            .count(&self.conn)
            .await? as i64;

        let folder_count: i64 = folders::Entity::find()
            .filter(folders::Column::ProfileId.eq(profile_id))
            .count(&self.conn)
            .await? as i64;

        Ok(session_count + folder_count)
    }

    async fn ensure_pinned_capacity(&self, profile_id: &str) -> Result<(), ChatDbError> {
        let profile = self.get_profile(profile_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Profile not found: {profile_id}"
            )))
        })?;
        let current = self.pinned_count(profile_id).await?;
        let limit = i64::from(profile.pinned_limit);
        if current >= limit {
            return Err(ChatDbError::PinnedLimitExceeded {
                profile_id: profile_id.to_string(),
                limit,
                current,
            });
        }
        Ok(())
    }

    async fn max_pin_sort_order(&self, profile_id: &str) -> Result<i32, ChatDbError> {
        let row = self
            .conn
            .query_one(sea_orm::Statement::from_string(
                self.conn.get_database_backend(),
                format!(
                    "SELECT COALESCE(MAX(pin_sort_order), -1) FROM sessions WHERE profile_id = '{}' AND pinned = 1",
                    profile_id.replace('\'', "''")
                ),
            ))
            .await?;
        let max_order = row
            .and_then(|r| r.try_get_by_index::<i32>(0).ok())
            .unwrap_or(-1);
        Ok(max_order)
    }

    // ── Scheduled Task CRUD ─────────────────────────────────

    /// Create a scheduled task.
    pub async fn create_scheduled_task(
        &self,
        params: CreateScheduledTaskParams<'_>,
    ) -> Result<scheduled_tasks::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active = scheduled_tasks::ActiveModel {
            id: Set(params.id.to_string()),
            profile_id: Set(params.profile_id.to_string()),
            session_id: Set(params.session_id.map(String::from)),
            name: Set(params.name.to_string()),
            trigger_kind: Set(params.trigger_kind.to_string()),
            trigger_type: Set(params.trigger_type.to_string()),
            cron_expression: Set(params.cron_expression.map(String::from)),
            run_at: Set(params.run_at.map(String::from)),
            script_path: Set(params.script_path.map(String::from)),
            script_inline: Set(params.script_inline.map(String::from)),
            needs_network: Set(params.needs_network),
            approved: Set(false),
            enabled: Set(true),
            last_run_at: Set(None),
            last_run_status: Set(None),
            last_run_output: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        };
        let model = active.insert(&self.conn).await?;
        Ok(model)
    }

    /// Get a scheduled task by ID.
    pub async fn get_scheduled_task(
        &self,
        id: &str,
    ) -> Result<Option<scheduled_tasks::Model>, ChatDbError> {
        let row = scheduled_tasks::Entity::find_by_id(id)
            .one(&self.conn)
            .await?;
        Ok(row)
    }

    /// List all scheduled tasks, optionally filtered by profile.
    pub async fn list_scheduled_tasks(
        &self,
        profile_id: Option<&str>,
    ) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        let mut query =
            scheduled_tasks::Entity::find().order_by_desc(scheduled_tasks::Column::CreatedAt);
        if let Some(pid) = profile_id {
            query = query.filter(scheduled_tasks::Column::ProfileId.eq(pid));
        }
        let rows = query.all(&self.conn).await?;
        Ok(rows)
    }

    /// Update a scheduled task's mutable fields.
    pub async fn update_scheduled_task(
        &self,
        id: &str,
        params: UpdateScheduledTaskParams<'_>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut active = scheduled_tasks::ActiveModel {
            id: Set(id.to_string()),
            updated_at: Set(now),
            ..Default::default()
        };
        if let Some(name) = params.name {
            active.name = Set(name.to_string());
        }
        if let Some(enabled) = params.enabled {
            active.enabled = Set(enabled);
        }
        if let Some(approved) = params.approved {
            active.approved = Set(approved);
        }
        if let Some(cron_expression) = params.cron_expression {
            active.cron_expression = Set(cron_expression.map(String::from));
        }
        if let Some(run_at) = params.run_at {
            active.run_at = Set(run_at.map(String::from));
        }
        if let Some(script_path) = params.script_path {
            active.script_path = Set(script_path.map(String::from));
        }
        if let Some(script_inline) = params.script_inline {
            active.script_inline = Set(script_inline.map(String::from));
        }
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Delete a scheduled task.
    pub async fn delete_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        scheduled_tasks::Entity::delete_by_id(id)
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    /// Approve a task for execution.
    pub async fn approve_scheduled_task(&self, id: &str) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active = scheduled_tasks::ActiveModel {
            id: Set(id.to_string()),
            approved: Set(true),
            updated_at: Set(now),
            ..Default::default()
        };
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Record a task execution result.
    pub async fn record_task_run(
        &self,
        id: &str,
        status: &str,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active = scheduled_tasks::ActiveModel {
            id: Set(id.to_string()),
            last_run_at: Set(Some(now.clone())),
            last_run_status: Set(Some(status.to_string())),
            last_run_output: Set(output.map(String::from)),
            updated_at: Set(now),
            ..Default::default()
        };
        active.update(&self.conn).await?;
        Ok(())
    }

    /// List enabled and approved tasks.
    ///
    /// Due-ness is resolved by the scheduler runtime because it depends on
    /// trigger-kind-specific rules.
    pub async fn list_due_tasks(&self) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        let rows = scheduled_tasks::Entity::find()
            .filter(scheduled_tasks::Column::Enabled.eq(true))
            .filter(scheduled_tasks::Column::Approved.eq(true))
            .all(&self.conn)
            .await?;
        Ok(rows)
    }

    // ── Push subscriptions ────────────────────────────────

    /// Store a push subscription from a browser.
    pub async fn add_push_subscription(
        &self,
        id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
    ) -> Result<push_subscriptions::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let model = push_subscriptions::ActiveModel {
            id: Set(id.to_string()),
            endpoint: Set(endpoint.to_string()),
            p256dh: Set(p256dh.to_string()),
            auth: Set(auth.to_string()),
            created_at: Set(now),
        };
        let result = model.insert(&self.conn).await?;
        Ok(result)
    }

    /// Remove a push subscription by endpoint.
    pub async fn remove_push_subscription(&self, endpoint: &str) -> Result<(), ChatDbError> {
        push_subscriptions::Entity::delete_many()
            .filter(push_subscriptions::Column::Endpoint.eq(endpoint))
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    /// List all push subscriptions.
    pub async fn list_push_subscriptions(
        &self,
    ) -> Result<Vec<push_subscriptions::Model>, ChatDbError> {
        let rows = push_subscriptions::Entity::find().all(&self.conn).await?;
        Ok(rows)
    }

    // ── Profile memories ──────────────────────────────────

    /// Get a profile memory by profile ID and type.
    pub async fn get_profile_memory(
        &self,
        profile_id: &str,
        memory_type: &str,
    ) -> Result<Option<profile_memories::Model>, ChatDbError> {
        let row = profile_memories::Entity::find()
            .filter(profile_memories::Column::ProfileId.eq(profile_id))
            .filter(profile_memories::Column::MemoryType.eq(memory_type))
            .one(&self.conn)
            .await?;
        Ok(row)
    }

    /// Get all memories for a profile.
    pub async fn get_profile_memories(
        &self,
        profile_id: &str,
    ) -> Result<Vec<profile_memories::Model>, ChatDbError> {
        let rows = profile_memories::Entity::find()
            .filter(profile_memories::Column::ProfileId.eq(profile_id))
            .all(&self.conn)
            .await?;
        Ok(rows)
    }

    /// Upsert a profile memory (insert or replace).
    pub async fn upsert_profile_memory(
        &self,
        profile_id: &str,
        memory_type: &str,
        content: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();

        // Try to find existing
        let existing = self.get_profile_memory(profile_id, memory_type).await?;

        if let Some(existing) = existing {
            // Update
            let mut active: profile_memories::ActiveModel = existing.into();
            active.content = Set(content.to_string());
            active.updated_at = Set(now);
            active.update(&self.conn).await?;
        } else {
            // Insert
            let id = uuid::Uuid::new_v4().to_string();
            let model = profile_memories::ActiveModel {
                id: Set(id),
                profile_id: Set(profile_id.to_string()),
                memory_type: Set(memory_type.to_string()),
                content: Set(content.to_string()),
                updated_at: Set(now),
            };
            model.insert(&self.conn).await?;
        }

        Ok(())
    }
}

/// Parameters for creating a scheduled task.
pub struct CreateScheduledTaskParams<'a> {
    pub id: &'a str,
    pub profile_id: &'a str,
    pub session_id: Option<&'a str>,
    pub name: &'a str,
    pub trigger_kind: &'a str,
    pub trigger_type: &'a str,
    pub cron_expression: Option<&'a str>,
    pub run_at: Option<&'a str>,
    pub script_path: Option<&'a str>,
    pub script_inline: Option<&'a str>,
    pub needs_network: bool,
}

/// Parameters for updating a scheduled task.
pub struct UpdateScheduledTaskParams<'a> {
    pub name: Option<&'a str>,
    pub enabled: Option<bool>,
    pub approved: Option<bool>,
    pub cron_expression: Option<Option<&'a str>>,
    pub run_at: Option<Option<&'a str>>,
    pub script_path: Option<Option<&'a str>>,
    pub script_inline: Option<Option<&'a str>>,
}

const PREVIEW_MAX_CHARS: usize = 100;

/// Truncate content to a preview string.
fn truncate_preview(content: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match trimmed.char_indices().nth(PREVIEW_MAX_CHARS) {
        Some((pos, _)) => format!("{}...", &trimmed[..pos]),
        None => trimmed.to_string(),
    }
}

/// Permission request data stored as JSON in message content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionRequestPayload {
    pub id: String,
    pub mode: String,
    pub script: String,
}

/// Permission response data stored as JSON in message content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionResponsePayload {
    pub id: String,
    pub approved: bool,
}

/// Ask-user request data stored as JSON in message content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserRequestPayload {
    pub id: String,
    pub questions: serde_json::Value,
}

/// Ask-user response data stored as JSON in message content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AskUserResponsePayload {
    pub id: String,
    pub answers: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{ChatDb, CreateProfileParams};

    #[tokio::test]
    async fn archive_and_unarchive_reset_pin_state_and_capacity() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let profile_id = "profile-test";
        db.create_profile(CreateProfileParams {
            id: profile_id,
            name: "Test",
            environment: "local",
            container_image: None,
            slug: None,
            icon: None,
            persona: None,
            preferences_json: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");

        let session_id = "session-test";
        db.create_session_in_profile(
            session_id,
            Some("Test session"),
            Some("test-model"),
            Some(profile_id),
        )
        .await
        .expect("session should be created");

        db.pin_session(session_id)
            .await
            .expect("session should be pinned");
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            1
        );

        db.archive_session(session_id)
            .await
            .expect("session should be archived");
        let archived = db
            .get_session(session_id)
            .await
            .expect("session fetch should succeed")
            .expect("session should exist");
        assert!(archived.archived, "session must be archived");
        assert!(
            !archived.pinned,
            "archived session must be removed from pinned list"
        );
        assert!(
            archived.folder_id.is_none(),
            "archived session must not keep folder assignment"
        );
        assert!(
            archived.pin_sort_order.is_none(),
            "archived session must not keep pin order"
        );
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            0
        );

        db.unarchive_session(session_id)
            .await
            .expect("session should be unarchived");
        let reopened = db
            .get_session(session_id)
            .await
            .expect("session fetch should succeed")
            .expect("session should exist");
        assert!(
            !reopened.archived,
            "unarchived session must return to regular list"
        );
        assert!(
            !reopened.pinned,
            "unarchived session must not auto-return to pinned"
        );
        assert!(
            reopened.folder_id.is_none(),
            "unarchived session must not keep folder assignment"
        );
        assert!(
            reopened.pin_sort_order.is_none(),
            "unarchived session must not keep pin order"
        );
        assert_eq!(
            db.pinned_count(profile_id)
                .await
                .expect("pinned count should load"),
            0
        );
    }

    #[tokio::test]
    async fn move_to_folder_requires_pinned_and_non_archived_session() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let profile_id = "profile-test";
        db.create_profile(CreateProfileParams {
            id: profile_id,
            name: "Test",
            environment: "local",
            container_image: None,
            slug: None,
            icon: None,
            persona: None,
            preferences_json: None,
            location_aware: false,
        })
        .await
        .expect("profile should be created");

        let session_id = "session-test";
        db.create_session_in_profile(
            session_id,
            Some("Test session"),
            Some("test-model"),
            Some(profile_id),
        )
        .await
        .expect("session should be created");

        let folder_id = "folder-test";
        db.create_folder(folder_id, profile_id, "Pinned Folder")
            .await
            .expect("folder should be created");

        let unpinned_err = db
            .move_session_to_folder(session_id, Some(folder_id))
            .await
            .expect_err("moving unpinned session into folder must fail");
        assert!(
            unpinned_err
                .to_string()
                .contains("cannot move unpinned session"),
            "unexpected error: {unpinned_err}"
        );

        db.pin_session(session_id)
            .await
            .expect("session should be pinned");
        db.archive_session(session_id)
            .await
            .expect("session should be archived");

        let archived_err = db
            .move_session_to_folder(session_id, Some(folder_id))
            .await
            .expect_err("moving archived session into folder must fail");
        assert!(
            archived_err
                .to_string()
                .contains("cannot move archived session"),
            "unexpected error: {archived_err}"
        );
    }

    #[tokio::test]
    async fn ensure_default_profiles_corrects_legacy_rows_without_duplicates() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_profile(CreateProfileParams {
            id: "legacy-daily",
            name: "Daily",
            environment: "container",
            container_image: Some("legacy-image"),
            slug: None,
            icon: Some("X"),
            persona: None,
            preferences_json: None,
            location_aware: false,
        })
        .await
        .expect("legacy daily should be created");
        db.create_profile(CreateProfileParams {
            id: "legacy-work",
            name: "Work",
            environment: "container",
            container_image: Some("legacy-image"),
            slug: None,
            icon: Some("Y"),
            persona: None,
            preferences_json: None,
            location_aware: true,
        })
        .await
        .expect("legacy work should be created");
        db.create_profile(CreateProfileParams {
            id: "legacy-local",
            name: "Local",
            environment: "local",
            container_image: Some("should-clear"),
            slug: None,
            icon: Some("Z"),
            persona: None,
            preferences_json: None,
            location_aware: true,
        })
        .await
        .expect("legacy local should be created");

        drop(db);
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should reopen successfully with migrations");

        let first_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should be corrected");
        assert_eq!(first_id, "legacy-daily");

        let all = db.list_profiles().await.expect("profiles should be listed");
        assert_eq!(all.len(), 3, "legacy rows should be corrected in-place");

        let daily = db
            .get_profile_by_slug("daily")
            .await
            .expect("daily query should succeed")
            .expect("daily profile should exist");
        assert_eq!(daily.id, "legacy-daily");
        assert_eq!(daily.name, "Daily");
        assert_eq!(daily.environment, "container");
        assert_eq!(daily.icon.as_deref(), Some("\u{1F305}"));
        assert!(daily.location_aware);

        let work = db
            .get_profile_by_slug("work")
            .await
            .expect("work query should succeed")
            .expect("work profile should exist");
        assert_eq!(work.id, "legacy-work");
        assert_eq!(work.name, "Work");
        assert_eq!(work.environment, "container");
        assert_eq!(work.icon.as_deref(), Some("\u{1F4BC}"));
        assert!(!work.location_aware);

        let local = db
            .get_profile_by_slug("local")
            .await
            .expect("local query should succeed")
            .expect("local profile should exist");
        let expected_hostname = hostname::get()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "local".to_string());
        assert_eq!(local.id, "legacy-local");
        assert_eq!(local.name, expected_hostname);
        assert_eq!(local.environment, "local");
        assert_eq!(local.icon.as_deref(), Some("\u{1F5A5}\u{FE0F}"));
        assert!(!local.location_aware);
        assert!(local.container_image.is_none());
    }
}
