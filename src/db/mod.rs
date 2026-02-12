//! Chat history database backed by SQLite via SeaORM.
//!
//! Single global database at `~/.may/history.db` containing all sessions and messages.

pub mod entity;

use std::path::Path;

use sea_orm::ConnectionTrait;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectOptions, Database, DatabaseConnection, DbErr,
    EntityTrait, Order, QueryFilter, QueryOrder, Schema, Set,
    sea_query::{Expr, Query, SqliteQueryBuilder},
};

use entity::{messages, sessions};

/// Error type for database operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatDbError {
    #[error("Database error: {0}")]
    Db(#[from] DbErr),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
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

        // Migrate: add new columns if they don't exist (idempotent)
        // SQLite doesn't support IF NOT EXISTS for ALTER TABLE ADD COLUMN,
        // so we check the error and ignore "duplicate column" errors.
        for stmt in [
            "ALTER TABLE sessions ADD COLUMN preview TEXT",
            "ALTER TABLE sessions ADD COLUMN message_count INTEGER NOT NULL DEFAULT 0",
        ] {
            if let Err(e) = conn.execute_unprepared(stmt).await {
                let msg = e.to_string();
                if !msg.contains("duplicate column") {
                    return Err(ChatDbError::Db(e));
                }
            }
        }

        // Create indexes
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_messages_session_role ON messages(session_id, role)",
        )
        .await?;

        // Enable WAL mode for better concurrent read/write performance
        conn.execute_unprepared("PRAGMA journal_mode=WAL").await?;

        Ok(Self { conn })
    }

    /// Get the underlying database connection.
    pub fn connection(&self) -> &DatabaseConnection {
        &self.conn
    }

    // ── Session CRUD ─────────────────────────────────────────────

    /// Create a new session.
    pub async fn create_session(
        &self,
        id: &str,
        title: Option<&str>,
        model: Option<&str>,
    ) -> Result<sessions::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active = sessions::ActiveModel {
            id: Set(id.to_string()),
            title: Set(title.map(String::from)),
            model: Set(model.map(String::from)),
            preview: Set(None),
            message_count: Set(0),
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
        sessions::Entity::delete_by_id(id).exec(&self.conn).await?;
        Ok(())
    }

    /// Delete all sessions.
    pub async fn delete_all_sessions(&self) -> Result<u64, ChatDbError> {
        let result = messages::Entity::delete_many().exec(&self.conn).await?;
        let session_result = sessions::Entity::delete_many().exec(&self.conn).await?;
        Ok(result.rows_affected + session_result.rows_affected)
    }

    /// List all sessions ordered by updated_at descending.
    pub async fn list_sessions(&self) -> Result<Vec<sessions::Model>, ChatDbError> {
        let sessions = sessions::Entity::find()
            .order_by(sessions::Column::UpdatedAt, Order::Desc)
            .all(&self.conn)
            .await?;
        Ok(sessions)
    }

    /// Set the `created_at` timestamp for a session (used during migration).
    pub async fn set_session_created_at(
        &self,
        id: &str,
        created_at: &str,
    ) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::CreatedAt, created_at);
        update.and_where(Expr::col(sessions::Column::Id).eq(id));
        let stmt = update.to_owned();
        let builder = self.conn.get_database_backend();
        self.conn.execute(builder.build(&stmt)).await?;
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
    ) -> Result<messages::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();

        // Insert the message
        let active = messages::ActiveModel {
            session_id: Set(session_id.to_string()),
            role: Set(role.to_string()),
            content: Set(content.to_string()),
            timestamp: Set(timestamp.map(String::from).or(Some(now.clone()))),
            tool_call_id: Set(tool_call_id.map(String::from)),
            ..Default::default()
        };
        let model = active.insert(&self.conn).await?;

        // Update session: touch updated_at, increment message_count, set preview
        let preview = truncate_preview(content);
        self.update_session_stats(session_id, &now, &preview, true)
            .await?;

        Ok(model)
    }

    /// Bulk-insert messages for a session (used during migration).
    pub async fn bulk_insert_messages(
        &self,
        session_id: &str,
        messages: &[BulkMessage<'_>],
    ) -> Result<(), ChatDbError> {
        for msg in messages {
            let active = messages::ActiveModel {
                session_id: Set(session_id.to_string()),
                role: Set(msg.role.to_string()),
                content: Set(msg.content.to_string()),
                timestamp: Set(msg.timestamp.map(String::from)),
                tool_call_id: Set(msg.tool_call_id.map(String::from)),
                ..Default::default()
            };
            active.insert(&self.conn).await?;
        }

        // Update session stats
        let count = messages.len() as i32;
        let preview = messages
            .last()
            .map(|m| truncate_preview(m.content))
            .unwrap_or_default();
        let now = jiff::Timestamp::now().to_string();

        let active = sessions::ActiveModel {
            id: Set(session_id.to_string()),
            updated_at: Set(now),
            message_count: Set(count),
            preview: Set(Some(preview)),
            ..Default::default()
        };
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
}

/// Message data for bulk insertion during migration.
pub struct BulkMessage<'a> {
    pub role: &'a str,
    pub content: &'a str,
    pub timestamp: Option<&'a str>,
    pub tool_call_id: Option<&'a str>,
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
