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
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Schema, Set, TransactionTrait,
    sea_query::{Expr, Query, SqliteQueryBuilder},
};

use crate::PermissionHistoryMode;
use crate::scheduler::TaskRunStatus;
use crate::session::{
    BrowserInteractionState, ChatMessage, ChatMessageStats, MessageRole, PendingUserMessage,
    PersistedAskUserRequest, PersistedPermissionRequest, PersistedWorkspaceRequest,
    SessionInteractionState, SessionPlanState, SessionRuntimeState, SessionRuntimeSummary,
};
use crate::task_trigger::TRIGGER_KIND_WEBHOOK;
use crate::xml_fragment::XmlFragment;
use entity::{
    context_debug_snapshots, folders, messages, network_access_logs, notification_mailbox,
    profile_memories, profile_memory_jobs, profiles, push_subscriptions, request_logs,
    scheduled_task_runs, scheduled_tasks, session_context_snapshots, session_pending_ask_user,
    session_pending_messages, session_pending_permissions, session_pending_workspace,
    session_plans, session_run_leases, session_runtimes, sessions,
};

/// Error type for database operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatDbError {
    #[error("Database error: {0}")]
    Db(#[from] DbErr),
    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid XML fragment: {0}")]
    InvalidXml(#[from] crate::xml_fragment::XmlFragmentError),
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
    pub cache_read_tokens: Option<u32>,
    pub cache_write_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub elapsed_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct NetworkAccessLogParams<'a> {
    pub session_id: Option<&'a str>,
    pub profile_id: Option<&'a str>,
    pub source: &'a str,
    pub mode: &'a str,
    pub domain: &'a str,
    pub port: u16,
    pub allowed: bool,
    pub reason: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct RequestLogParams<'a> {
    pub session_id: &'a str,
    pub timestamp: &'a str,
    pub model: &'a str,
    pub provider_id: &'a str,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub total_tokens: Option<u32>,
    pub cache_read_tokens: Option<u32>,
    pub cache_write_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub elapsed_ms: u64,
    pub ok: bool,
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantCacheUsageSummary {
    pub assistant_message_count: u64,
    pub cache_hit_message_count: u64,
    pub prompt_tokens_total: u64,
    pub cache_read_tokens_total: u64,
    pub cache_write_tokens_total: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingPermissionLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingAskUserLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingWorkspaceLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone)]
pub struct NotificationCallbackConsumeResult {
    pub id: String,
    pub session_id: Option<String>,
    pub callback_context_xml: Option<String>,
    pub callback_message: Option<String>,
    pub has_callback: bool,
    pub callback_consumed: bool,
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
    Remote,
}

fn parse_profile_environment(value: &str) -> Result<ProfileEnvironmentKind, ChatDbError> {
    match value {
        "local" => Ok(ProfileEnvironmentKind::Local),
        "container" => Ok(ProfileEnvironmentKind::Container),
        "remote" => Ok(ProfileEnvironmentKind::Remote),
        other => Err(ChatDbError::InvalidProfileEnvironment(other.to_string())),
    }
}

fn prefixed_text(prefix: &str, suffix: &str) -> String {
    let mut text = String::with_capacity(prefix.len() + suffix.len());
    text.push_str(prefix);
    text.push_str(suffix);
    text
}

fn db_custom_error(prefix: &str, error: &impl std::fmt::Display) -> DbErr {
    let error_text = error.to_string();
    DbErr::Custom(prefixed_text(prefix, error_text.as_str()))
}

fn session_not_found_db_error(session_id: &str) -> ChatDbError {
    ChatDbError::Db(DbErr::RecordNotFound(prefixed_text(
        "Session not found: ",
        session_id,
    )))
}

fn message_stats_params_from_chat_stats(stats: &ChatMessageStats) -> MessageStatsParams {
    MessageStatsParams {
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
    /// Creates the current schema if the tables don't exist.
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

        let task_runs_stmt = schema.create_table_from_entity(scheduled_task_runs::Entity);
        let create_sql = task_runs_stmt
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

        let memory_jobs_stmt = schema.create_table_from_entity(profile_memory_jobs::Entity);
        let create_sql = memory_jobs_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        // Context debug snapshots table
        let context_debug_stmt = schema.create_table_from_entity(context_debug_snapshots::Entity);
        let create_sql = context_debug_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let notification_mailbox_stmt =
            schema.create_table_from_entity(notification_mailbox::Entity);
        let create_sql = notification_mailbox_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let request_logs_stmt = schema.create_table_from_entity(request_logs::Entity);
        let create_sql = request_logs_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let network_access_logs_stmt = schema.create_table_from_entity(network_access_logs::Entity);
        let create_sql = network_access_logs_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let session_context_stmt =
            schema.create_table_from_entity(session_context_snapshots::Entity);
        let create_sql = session_context_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let session_runtime_stmt = schema.create_table_from_entity(session_runtimes::Entity);
        let create_sql = session_runtime_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let session_plan_stmt = schema.create_table_from_entity(session_plans::Entity);
        let create_sql = session_plan_stmt
            .to_string(SqliteQueryBuilder)
            .replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS");
        conn.execute_unprepared(&create_sql).await?;

        let session_run_lease_stmt = schema.create_table_from_entity(session_run_leases::Entity);
        let create_sql = session_run_lease_stmt
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

        let pending_workspace_stmt =
            schema.create_table_from_entity(session_pending_workspace::Entity);
        let create_sql = pending_workspace_stmt
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
            "CREATE INDEX IF NOT EXISTS idx_profile_memory_jobs_dirty_updated_at ON profile_memory_jobs(dirty, updated_at)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_context_debug_snapshots_timestamp ON context_debug_snapshots(timestamp)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_notification_mailbox_status_created_at ON notification_mailbox(status, created_at)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_timestamp ON request_logs(timestamp)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_session_timestamp ON request_logs(session_id, timestamp)",
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
            "CREATE INDEX IF NOT EXISTS idx_network_access_logs_timestamp ON network_access_logs(timestamp)",
        )
        .await?;
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_network_access_logs_session ON network_access_logs(session_id, timestamp)",
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
        conn.execute_unprepared(
            "CREATE INDEX IF NOT EXISTS idx_session_pending_workspace_session ON session_pending_workspace(session_id)",
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
            .order_by(session_pending_messages::Column::Id, Order::Asc)
            .all(&self.conn)
            .await?;
        let permission_row = session_pending_permissions::Entity::find()
            .filter(session_pending_permissions::Column::SessionId.eq(session_id))
            .one(&self.conn)
            .await?;
        let workspace_row = session_pending_workspace::Entity::find()
            .filter(session_pending_workspace::Column::SessionId.eq(session_id))
            .one(&self.conn)
            .await?;
        let ask_user_row = session_pending_ask_user::Entity::find()
            .filter(session_pending_ask_user::Column::SessionId.eq(session_id))
            .one(&self.conn)
            .await?;
        let plan_row = session_plans::Entity::find_by_id(session_id)
            .one(&self.conn)
            .await?;

        let mut steer_messages = Vec::new();
        let mut queued_messages = Vec::new();
        for row in message_rows {
            let message = PendingUserMessage {
                content: row.content,
                attachments: serde_json::from_str(&row.attachments_json)?,
                hidden_context_xml: row.hidden_context_xml.map(XmlFragment::parse).transpose()?,
                created_at: row.created_at,
                delivery_mode: row.delivery_mode.parse().unwrap_or_default(),
            };
            if message.delivery_mode == crate::session::PendingUserMessageMode::Steer {
                steer_messages.push(message);
            } else {
                queued_messages.push(message);
            }
        }
        steer_messages.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        let pending_immediate = if steer_messages.is_empty() {
            None
        } else {
            Some(steer_messages.remove(0))
        };
        queued_messages.splice(0..0, steer_messages);

        let mut summary = SessionRuntimeSummary {
            state: runtime_row
                .as_ref()
                .map(|row| row.state.as_str())
                .unwrap_or(SessionRuntimeState::Idle.as_str())
                .parse()
                .unwrap_or(SessionRuntimeState::Idle),
            plan_state: plan_row
                .as_ref()
                .map(|row| row.plan_state.as_str())
                .unwrap_or(SessionPlanState::Disabled.as_str())
                .parse()
                .unwrap_or(SessionPlanState::Disabled),
            interactions: SessionInteractionState {
                browser: BrowserInteractionState {
                    takeover_active: runtime_row
                        .as_ref()
                        .is_some_and(|row| row.browser_takeover_active),
                    takeover_alert_pending: runtime_row
                        .as_ref()
                        .is_some_and(|row| row.browser_takeover_alert_pending),
                },
            },
            pending_immediate,
            queued_messages,
            waiting_permission: permission_row.map(|row| PersistedPermissionRequest {
                id: row.request_id,
                request: crate::session::PersistedPermissionRequestPayload {
                    mode: row
                        .mode
                        .parse()
                        .expect("persisted permission request mode must be valid"),
                    script: row.script,
                    session_id: Some(row.session_id),
                },
            }),
            waiting_workspace: workspace_row.map(|row| PersistedWorkspaceRequest {
                id: row.request_id,
                request: crate::session::PersistedWorkspaceRequestPayload {
                    path: row.path,
                    reason: row.reason,
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
            browser_takeover_active: Set(summary.interactions.browser.takeover_active),
            browser_takeover_alert_pending: Set(summary
                .interactions
                .browser
                .takeover_alert_pending),
            updated_at: Set(now.clone()),
        };
        session_runtimes::Entity::insert(active_runtime)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(session_runtimes::Column::SessionId)
                    .update_columns([
                        session_runtimes::Column::State,
                        session_runtimes::Column::BrowserTakeoverActive,
                        session_runtimes::Column::BrowserTakeoverAlertPending,
                        session_runtimes::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.conn)
            .await?;

        let active_plan = session_plans::ActiveModel {
            session_id: Set(session_id.to_string()),
            plan_state: Set(summary.plan_state.as_str().to_string()),
            updated_at: Set(now),
        };
        session_plans::Entity::insert(active_plan)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(session_plans::Column::SessionId)
                    .update_columns([
                        session_plans::Column::PlanState,
                        session_plans::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.conn)
            .await?;

        session_pending_messages::Entity::delete_many()
            .filter(session_pending_messages::Column::SessionId.eq(session_id))
            .exec(&self.conn)
            .await?;

        for message in &summary.queued_messages {
            session_pending_messages::ActiveModel {
                id: NotSet,
                session_id: Set(session_id.to_string()),
                delivery_mode: Set(message.delivery_mode.as_str().to_string()),
                content: Set(message.content.clone()),
                attachments_json: Set(serde_json::to_string(&message.attachments)?),
                hidden_context_xml: Set(message
                    .hidden_context_xml
                    .clone()
                    .map(XmlFragment::into_inner)),
                created_at: Set(message.created_at.clone()),
            }
            .insert(&self.conn)
            .await?;
        }
        if let Some(message) = &summary.pending_immediate {
            session_pending_messages::ActiveModel {
                id: NotSet,
                session_id: Set(session_id.to_string()),
                delivery_mode: Set(message.delivery_mode.as_str().to_string()),
                content: Set(message.content.clone()),
                attachments_json: Set(serde_json::to_string(&message.attachments)?),
                hidden_context_xml: Set(message
                    .hidden_context_xml
                    .clone()
                    .map(XmlFragment::into_inner)),
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

        session_pending_workspace::Entity::delete_many()
            .filter(session_pending_workspace::Column::SessionId.eq(session_id))
            .exec(&self.conn)
            .await?;
        if let Some(request) = &summary.waiting_workspace {
            session_pending_workspace::ActiveModel {
                request_id: Set(request.id.clone()),
                session_id: Set(session_id.to_string()),
                path: Set(request.request.path.clone()),
                reason: Set(request.request.reason.clone()),
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

    pub async fn set_session_plan_state(
        &self,
        session_id: &str,
        plan_state: SessionPlanState,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.plan_state = plan_state;
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    /// Enqueue a pending-immediate message while preserving older steer messages.
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
        if let Some(previous) = summary.pending_immediate.take() {
            summary.push_overflow_steer(previous);
        }
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

    /// Remove and return the next pending message only when follow-up execution is allowed.
    pub async fn take_next_dispatchable_pending_message(
        &self,
        session_id: &str,
    ) -> Result<Option<PendingUserMessage>, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let summary = self.load_session_runtime_summary(&session.id).await?;
        if !summary.can_dispatch_follow_up() {
            return Ok(None);
        }
        self.take_next_pending_message(session_id).await
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

    /// Persist a waiting workspace request for refresh-safe recovery.
    pub async fn set_waiting_workspace(
        &self,
        session_id: &str,
        request: Option<PersistedWorkspaceRequest>,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self.get_session(session_id).await?.ok_or_else(|| {
            ChatDbError::Db(DbErr::RecordNotFound(format!(
                "Session not found: {session_id}"
            )))
        })?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.waiting_workspace = request;
        if summary.waiting_workspace.is_none() {
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

    pub async fn set_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| session_not_found_db_error(session_id))?;
        session_runtimes::Entity::update_many()
            .col_expr(
                session_runtimes::Column::BrowserTakeoverActive,
                Expr::value(false),
            )
            .exec(&self.conn)
            .await?;

        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        summary.interactions.browser.takeover_active = true;
        summary.interactions.browser.takeover_alert_pending = false;
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    pub async fn clear_browser_takeover_owner(
        &self,
        session_id: &str,
    ) -> Result<SessionRuntimeSummary, ChatDbError> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| session_not_found_db_error(session_id))?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        if !summary.interactions.browser.takeover_active {
            return Ok(summary);
        }
        summary.interactions.browser.takeover_active = false;
        summary.interactions.browser.takeover_alert_pending = true;
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(summary)
    }

    pub async fn take_browser_takeover_alert_pending(
        &self,
        session_id: &str,
    ) -> Result<bool, ChatDbError> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| session_not_found_db_error(session_id))?;
        let mut summary = self.load_session_runtime_summary(&session.id).await?;
        let pending = summary.interactions.browser.takeover_alert_pending;
        if !pending {
            return Ok(false);
        }
        summary.interactions.browser.takeover_alert_pending = false;
        summary.normalize_state();
        self.write_session_runtime_summary(session_id, &summary)
            .await?;
        Ok(true)
    }

    pub async fn session_has_browser_takeover(
        &self,
        session_id: &str,
    ) -> Result<bool, ChatDbError> {
        let Some(summary) = self.get_session_runtime(session_id).await? else {
            return Ok(false);
        };
        Ok(summary.interactions.browser.takeover_active)
    }

    pub async fn find_browser_takeover_session(&self) -> Result<Option<String>, ChatDbError> {
        let row = session_runtimes::Entity::find()
            .filter(session_runtimes::Column::BrowserTakeoverActive.eq(true))
            .one(&self.conn)
            .await?;
        Ok(row.map(|model| model.session_id))
    }

    pub(crate) async fn find_pending_permission_request(
        &self,
        request_id: &str,
    ) -> Result<Option<PendingPermissionLookup>, ChatDbError> {
        let Some(row) = session_pending_permissions::Entity::find_by_id(request_id)
            .one(&self.conn)
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(PendingPermissionLookup {
            session_id: row.session_id,
        }))
    }

    pub(crate) async fn find_pending_workspace_request(
        &self,
        request_id: &str,
    ) -> Result<Option<PendingWorkspaceLookup>, ChatDbError> {
        let Some(row) = session_pending_workspace::Entity::find_by_id(request_id)
            .one(&self.conn)
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(PendingWorkspaceLookup {
            session_id: row.session_id,
        }))
    }

    pub(crate) async fn find_pending_ask_user_request(
        &self,
        request_id: &str,
    ) -> Result<Option<PendingAskUserLookup>, ChatDbError> {
        let Some(row) = session_pending_ask_user::Entity::find_by_id(request_id)
            .one(&self.conn)
            .await?
        else {
            return Ok(None);
        };

        Ok(Some(PendingAskUserLookup {
            session_id: row.session_id,
        }))
    }

    pub async fn session_has_approved_permission_mode(
        &self,
        session_id: &str,
        mode: &str,
    ) -> Result<bool, ChatDbError> {
        let responded_ids = self
            .get_messages_by_role(session_id, MessageRole::PermissionResponse.as_str())
            .await?
            .into_iter()
            .map(|row| {
                serde_json::from_str::<crate::session::history::PermissionResponseData>(
                    &row.content,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|response| response.approved)
            .map(|response| response.id)
            .collect::<std::collections::HashSet<_>>();

        let approved = self
            .get_messages_by_role(session_id, MessageRole::PermissionRequest.as_str())
            .await?
            .into_iter()
            .map(|row| {
                serde_json::from_str::<crate::session::history::PermissionRequestData>(&row.content)
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|request| request.mode == mode && responded_ids.contains(&request.id));

        Ok(approved)
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
            let original_browser_takeover = summary.interactions.browser.takeover_active;
            let expired_terminal_wait =
                matches!(original_state, SessionRuntimeState::WaitingTerminalInput);

            if expired_terminal_wait {
                summary.state = SessionRuntimeState::Idle;
            }
            if original_browser_takeover {
                summary.interactions.browser.takeover_active = false;
                summary.interactions.browser.takeover_alert_pending = true;
            }
            summary.normalize_state();
            if matches!(original_state, SessionRuntimeState::Running)
                && summary.state == SessionRuntimeState::Running
            {
                summary.state = SessionRuntimeState::Idle;
            }

            if summary.state != original_state || expired_terminal_wait || original_browser_takeover
            {
                self.write_session_runtime_summary(&session_id, &summary)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn recover_task_runs_after_restart(&self) -> Result<(), ChatDbError> {
        let running = scheduled_task_runs::Entity::find()
            .filter(scheduled_task_runs::Column::Status.eq(TaskRunStatus::Running.as_str()))
            .all(&self.conn)
            .await?;
        let finished_at = jiff::Timestamp::now().to_string();
        for run in running {
            let mut active: scheduled_task_runs::ActiveModel = run.into();
            active.status = Set(TaskRunStatus::Interrupted.as_str().to_string());
            active.output = Set(Some(
                "May server restarted before the task run completed".to_string(),
            ));
            active.finished_at = Set(Some(finished_at.clone()));
            active.updated_at = Set(finished_at.clone());
            active.update(&self.conn).await?;
        }
        Ok(())
    }

    pub async fn recover_session_run_leases_after_restart(&self) -> Result<(), ChatDbError> {
        session_run_leases::Entity::update_many()
            .col_expr(session_run_leases::Column::Active, Expr::value(false))
            .col_expr(
                session_run_leases::Column::CancellationRequested,
                Expr::value(false),
            )
            .col_expr(
                session_run_leases::Column::UpdatedAt,
                Expr::value(jiff::Timestamp::now().to_string()),
            )
            .filter(session_run_leases::Column::Active.eq(true))
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    pub async fn claim_session_run_lease(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let session_id_owned = session_id.to_string();
        self.conn
            .transaction::<_, bool, DbErr>(|txn| {
                Box::pin(async move {
                    let existing =
                        session_run_leases::Entity::find_by_id(session_id_owned.as_str())
                            .one(txn)
                            .await?;
                    if let Some(existing) = existing {
                        if existing.active {
                            return Ok(false);
                        }
                        let mut active: session_run_leases::ActiveModel = existing.into();
                        active.active = Set(true);
                        active.cancellation_requested = Set(false);
                        active.updated_at = Set(jiff::Timestamp::now().to_string());
                        active.update(txn).await?;
                    } else {
                        session_run_leases::ActiveModel {
                            session_id: Set(session_id_owned.clone()),
                            active: Set(true),
                            cancellation_requested: Set(false),
                            updated_at: Set(jiff::Timestamp::now().to_string()),
                        }
                        .insert(txn)
                        .await?;
                    }
                    Ok(true)
                })
            })
            .await
            .map_err(|error| match error {
                sea_orm::TransactionError::Connection(db_error)
                | sea_orm::TransactionError::Transaction(db_error) => ChatDbError::Db(db_error),
            })
    }

    pub async fn release_session_run_lease(&self, session_id: &str) -> Result<(), ChatDbError> {
        session_run_leases::Entity::update_many()
            .col_expr(session_run_leases::Column::Active, Expr::value(false))
            .col_expr(
                session_run_leases::Column::CancellationRequested,
                Expr::value(false),
            )
            .col_expr(
                session_run_leases::Column::UpdatedAt,
                Expr::value(jiff::Timestamp::now().to_string()),
            )
            .filter(session_run_leases::Column::SessionId.eq(session_id))
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    pub async fn request_session_run_cancel(&self, session_id: &str) -> Result<bool, ChatDbError> {
        let result = session_run_leases::Entity::update_many()
            .col_expr(
                session_run_leases::Column::CancellationRequested,
                Expr::value(true),
            )
            .col_expr(
                session_run_leases::Column::UpdatedAt,
                Expr::value(jiff::Timestamp::now().to_string()),
            )
            .filter(session_run_leases::Column::SessionId.eq(session_id))
            .filter(session_run_leases::Column::Active.eq(true))
            .exec(&self.conn)
            .await?;
        Ok(result.rows_affected > 0)
    }

    /// Return session ids that have persisted follow-up work ready to resume.
    pub async fn sessions_requiring_pending_dispatch(&self) -> Result<Vec<String>, ChatDbError> {
        let sessions = self.list_sessions().await?;
        let mut session_ids = Vec::new();
        for session in sessions {
            let summary = self.load_session_runtime_summary(&session.id).await?;
            if summary.can_dispatch_follow_up() {
                session_ids.push(session.id);
            }
        }
        Ok(session_ids)
    }

    /// Update session stats after inserting a message row.
    async fn update_session_stats(
        &self,
        session_id: &str,
        updated_at: &str,
        preview: Option<&str>,
        increment_count: bool,
    ) -> Result<(), ChatDbError> {
        let mut update = Query::update();
        update.table(sessions::Entity);
        update.value(sessions::Column::UpdatedAt, updated_at);
        if let Some(preview) = preview {
            update.value(sessions::Column::Preview, preview);
        }

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
        role: MessageRole,
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
        let cache_read_tokens = stats
            .and_then(|s| s.cache_read_tokens)
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("cache_read_tokens overflow: {e}")))?;
        let cache_write_tokens = stats
            .and_then(|s| s.cache_write_tokens)
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("cache_write_tokens overflow: {e}")))?;
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
        let session_update = role.session_summary_update();
        let role_str = role.as_str();

        // Insert the message
        let active = messages::ActiveModel {
            session_id: Set(session_id.to_string()),
            role: Set(role_str.to_string()),
            content: Set(content.to_string()),
            timestamp: Set(timestamp.map(String::from).or(Some(now.clone()))),
            tool_call_id: Set(tool_call_id.map(String::from)),
            provider_id: Set(provider_id),
            prompt_tokens: Set(prompt_tokens),
            completion_tokens: Set(completion_tokens),
            total_tokens: Set(total_tokens),
            cache_read_tokens: Set(cache_read_tokens),
            cache_write_tokens: Set(cache_write_tokens),
            context_window: Set(context_window),
            elapsed_ms: Set(elapsed_ms),
            ..Default::default()
        };
        let model = active.insert(&self.conn).await?;

        if session_update.touch_updated_at {
            let preview = session_update
                .update_preview
                .then(|| truncate_preview(content));
            self.update_session_stats(
                session_id,
                &now,
                preview.as_deref(),
                session_update.increment_message_count,
            )
            .await?;
        }

        Ok(model)
    }

    pub async fn append_chat_message(
        &self,
        session_id: &str,
        message: &ChatMessage,
    ) -> Result<messages::Model, ChatDbError> {
        let timestamp = message.timestamp.map(|value| value.to_string());
        let stats = message
            .stats
            .as_ref()
            .map(message_stats_params_from_chat_stats);
        self.append_message(
            session_id,
            message.role.clone(),
            &message.content,
            timestamp.as_deref(),
            message.tool_call_id.as_deref(),
            stats.as_ref(),
        )
        .await
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
        let cache_read_tokens = stats
            .cache_read_tokens
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("cache_read_tokens overflow: {e}")))?;
        let cache_write_tokens = stats
            .cache_write_tokens
            .map(i32::try_from)
            .transpose()
            .map_err(|e| DbErr::Custom(format!("cache_write_tokens overflow: {e}")))?;
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
        active.cache_read_tokens = Set(cache_read_tokens);
        active.cache_write_tokens = Set(cache_write_tokens);
        active.context_window = Set(context_window);
        active.elapsed_ms = Set(elapsed_ms);
        active.update(&self.conn).await?;
        Ok(())
    }

    pub async fn set_latest_assistant_chat_stats(
        &self,
        session_id: &str,
        stats: &ChatMessageStats,
    ) -> Result<(), ChatDbError> {
        self.set_latest_assistant_stats(session_id, &message_stats_params_from_chat_stats(stats))
            .await
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

    /// Count messages that contribute to the LLM-visible conversation history.
    pub async fn llm_message_count(&self, session_id: &str) -> Result<usize, ChatDbError> {
        let count = messages::Entity::find()
            .filter(messages::Column::SessionId.eq(session_id))
            .filter(messages::Column::Role.is_in([
                MessageRole::User.as_str(),
                MessageRole::Assistant.as_str(),
                MessageRole::System.as_str(),
                MessageRole::HiddenContext.as_str(),
                MessageRole::Tool.as_str(),
            ]))
            .count(&self.conn)
            .await?;
        usize::try_from(count).map_err(|error| ChatDbError::Db(DbErr::Custom(error.to_string())))
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

    /// Summarize persisted cache-token usage across assistant messages.
    pub async fn assistant_cache_usage_summary(
        &self,
    ) -> Result<AssistantCacheUsageSummary, ChatDbError> {
        let row = self
            .conn
            .query_one(sea_orm::Statement::from_sql_and_values(
                self.conn.get_database_backend(),
                "SELECT \
                    COUNT(*) AS assistant_message_count, \
                    COALESCE(SUM(CASE WHEN COALESCE(cache_read_tokens, 0) > 0 THEN 1 ELSE 0 END), 0) AS cache_hit_message_count, \
                    COALESCE(SUM(COALESCE(prompt_tokens, 0)), 0) AS prompt_tokens_total, \
                    COALESCE(SUM(COALESCE(cache_read_tokens, 0)), 0) AS cache_read_tokens_total, \
                    COALESCE(SUM(COALESCE(cache_write_tokens, 0)), 0) AS cache_write_tokens_total \
                 FROM messages \
                 WHERE role = ?",
                ["assistant".into()],
            ))
            .await?;

        let Some(row) = row else {
            return Ok(AssistantCacheUsageSummary::default());
        };

        Ok(AssistantCacheUsageSummary {
            assistant_message_count: query_u64(&row, 0)?,
            cache_hit_message_count: query_u64(&row, 1)?,
            prompt_tokens_total: query_u64(&row, 2)?,
            cache_read_tokens_total: query_u64(&row, 3)?,
            cache_write_tokens_total: query_u64(&row, 4)?,
        })
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
        snapshot: SessionContextSnapshotUpsert<'_>,
    ) -> Result<(), ChatDbError> {
        let updated_at = jiff::Timestamp::now().to_string();
        let active = session_context_snapshots::ActiveModel {
            session_id: Set(session_id.to_string()),
            context_json: Set(snapshot.context_json.to_string()),
            assembled_messages_json: Set(snapshot.assembled_messages_json.map(ToOwned::to_owned)),
            metadata_json: Set(snapshot.metadata_json.map(ToOwned::to_owned)),
            updated_at: Set(updated_at),
        };
        session_context_snapshots::Entity::insert(active)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(
                    session_context_snapshots::Column::SessionId,
                )
                .update_columns([
                    session_context_snapshots::Column::ContextJson,
                    session_context_snapshots::Column::AssembledMessagesJson,
                    session_context_snapshots::Column::MetadataJson,
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

    /// Insert a new mailbox notification entry.
    pub async fn create_notification_mailbox_entry(
        &self,
        id: &str,
        payload: &crate::notification::NotificationPayload,
        status: &str,
    ) -> Result<notification_mailbox::Model, ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        notification_mailbox::ActiveModel {
            id: Set(id.to_string()),
            title: Set(payload.title.clone()),
            body: Set(payload.body.clone()),
            session_id: Set(payload.session_id.clone()),
            callback_context_xml: Set(payload
                .callback_context_xml
                .clone()
                .map(XmlFragment::into_inner)),
            callback_message: Set(payload.callback_message.clone()),
            status: Set(status.to_string()),
            sent_count: Set(0),
            last_error: Set(None),
            delivered_at: Set(None),
            created_at: Set(now.clone()),
            updated_at: Set(now),
        }
        .insert(&self.conn)
        .await
        .map_err(Into::into)
    }

    /// Update mailbox delivery state after a send attempt.
    pub async fn update_notification_mailbox_delivery(
        &self,
        id: &str,
        status: &str,
        sent_count: usize,
        last_error: Option<&str>,
        delivered_at: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let mut active: notification_mailbox::ActiveModel =
            notification_mailbox::Entity::find_by_id(id)
                .one(&self.conn)
                .await?
                .ok_or_else(|| DbErr::RecordNotFound(id.to_string()))?
                .into();
        active.status = Set(status.to_string());
        active.sent_count = Set(i32::try_from(sent_count)
            .map_err(|error| db_custom_error("notification sent_count overflow: ", &error))?);
        active.last_error = Set(last_error.map(ToOwned::to_owned));
        active.delivered_at = Set(delivered_at.map(ToOwned::to_owned));
        active.updated_at = Set(jiff::Timestamp::now().to_string());
        active.update(&self.conn).await?;
        Ok(())
    }

    /// Return notifications that still need delivery attempts.
    pub async fn list_pending_notification_mailbox_entries(
        &self,
    ) -> Result<Vec<notification_mailbox::Model>, ChatDbError> {
        notification_mailbox::Entity::find()
            .filter(notification_mailbox::Column::Status.eq("pending"))
            .order_by_asc(notification_mailbox::Column::CreatedAt)
            .all(&self.conn)
            .await
            .map_err(Into::into)
    }

    pub async fn list_notification_mailbox_entries(
        &self,
        limit: u64,
    ) -> Result<Vec<notification_mailbox::Model>, ChatDbError> {
        notification_mailbox::Entity::find()
            .order_by_desc(notification_mailbox::Column::CreatedAt)
            .limit(limit)
            .all(&self.conn)
            .await
            .map_err(Into::into)
    }

    pub async fn get_notification_mailbox_entry(
        &self,
        id: &str,
    ) -> Result<Option<notification_mailbox::Model>, ChatDbError> {
        notification_mailbox::Entity::find_by_id(id)
            .one(&self.conn)
            .await
            .map_err(Into::into)
    }

    pub async fn mark_notification_mailbox_opened(&self, id: &str) -> Result<(), ChatDbError> {
        let mut active: notification_mailbox::ActiveModel =
            notification_mailbox::Entity::find_by_id(id)
                .one(&self.conn)
                .await?
                .ok_or_else(|| DbErr::RecordNotFound(id.to_string()))?
                .into();
        active.status = Set("opened".to_string());
        active.updated_at = Set(jiff::Timestamp::now().to_string());
        active.update(&self.conn).await?;
        Ok(())
    }

    pub async fn consume_notification_callback(
        &self,
        id: &str,
    ) -> Result<NotificationCallbackConsumeResult, ChatDbError> {
        let entry = self
            .get_notification_mailbox_entry(id)
            .await?
            .ok_or_else(|| DbErr::RecordNotFound(id.to_string()))?;
        let has_callback = entry.callback_context_xml.is_some() || entry.callback_message.is_some();
        let callback_consumed = has_callback && entry.status != "opened";
        if entry.status != "opened" {
            self.mark_notification_mailbox_opened(id).await?;
        }
        Ok(NotificationCallbackConsumeResult {
            id: entry.id,
            session_id: entry.session_id,
            callback_context_xml: entry.callback_context_xml,
            callback_message: entry.callback_message,
            has_callback,
            callback_consumed,
        })
    }

    pub async fn append_request_log(
        &self,
        params: RequestLogParams<'_>,
    ) -> Result<request_logs::Model, ChatDbError> {
        request_logs::ActiveModel {
            id: NotSet,
            session_id: Set(params.session_id.to_string()),
            timestamp: Set(params.timestamp.to_string()),
            model: Set(params.model.to_string()),
            provider_id: Set(params.provider_id.to_string()),
            prompt_tokens: Set(params
                .prompt_tokens
                .map(i32::try_from)
                .transpose()
                .map_err(|error| db_custom_error("prompt_tokens overflow: ", &error))?),
            completion_tokens: Set(params
                .completion_tokens
                .map(i32::try_from)
                .transpose()
                .map_err(|error| db_custom_error("completion_tokens overflow: ", &error))?),
            total_tokens: Set(params
                .total_tokens
                .map(i32::try_from)
                .transpose()
                .map_err(|error| db_custom_error("total_tokens overflow: ", &error))?),
            cache_read_tokens: Set(params
                .cache_read_tokens
                .map(i32::try_from)
                .transpose()
                .map_err(|error| db_custom_error("cache_read_tokens overflow: ", &error))?),
            cache_write_tokens: Set(params
                .cache_write_tokens
                .map(i32::try_from)
                .transpose()
                .map_err(|error| db_custom_error("cache_write_tokens overflow: ", &error))?),
            context_window: Set(params
                .context_window
                .map(i32::try_from)
                .transpose()
                .map_err(|error| db_custom_error("context_window overflow: ", &error))?),
            elapsed_ms: Set(i64::try_from(params.elapsed_ms)
                .map_err(|error| db_custom_error("elapsed_ms overflow: ", &error))?),
            ok: Set(params.ok),
            error: Set(params.error.map(ToOwned::to_owned)),
        }
        .insert(&self.conn)
        .await
        .map_err(Into::into)
    }

    pub async fn list_request_logs(
        &self,
        limit: u64,
    ) -> Result<Vec<request_logs::Model>, ChatDbError> {
        request_logs::Entity::find()
            .order_by_desc(request_logs::Column::Timestamp)
            .limit(limit)
            .all(&self.conn)
            .await
            .map_err(Into::into)
    }

    pub async fn append_network_access_log(
        &self,
        params: NetworkAccessLogParams<'_>,
    ) -> Result<network_access_logs::Model, ChatDbError> {
        let port = i32::from(params.port);
        network_access_logs::ActiveModel {
            id: NotSet,
            session_id: Set(params.session_id.map(ToOwned::to_owned)),
            profile_id: Set(params.profile_id.map(ToOwned::to_owned)),
            source: Set(params.source.to_string()),
            mode: Set(params.mode.to_string()),
            domain: Set(params.domain.to_string()),
            port: Set(port),
            allowed: Set(params.allowed),
            reason: Set(params.reason.map(ToOwned::to_owned)),
            timestamp: Set(jiff::Timestamp::now().to_string()),
        }
        .insert(&self.conn)
        .await
        .map_err(Into::into)
    }

    pub async fn list_network_access_logs(
        &self,
        limit: u64,
    ) -> Result<Vec<network_access_logs::Model>, ChatDbError> {
        network_access_logs::Entity::find()
            .order_by_desc(network_access_logs::Column::Timestamp)
            .limit(limit)
            .all(&self.conn)
            .await
            .map_err(Into::into)
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
                        && req_data.mode == PermissionHistoryMode::Network
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
                ProfileEnvironmentKind::Local | ProfileEnvironmentKind::Remote => None,
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
        if matches!(
            next_environment,
            Some(ProfileEnvironmentKind::Local | ProfileEnvironmentKind::Remote)
        ) {
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
        for def in &defaults {
            let ensured = if let Some(existing) = self.get_profile_by_slug(def.slug).await? {
                existing
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
        }

        // first_id is always set because we iterate at least one default profile
        Ok(first_id.expect("at least one default profile must be created"))
    }

    async fn max_profile_sort_order(&self) -> Result<i32, ChatDbError> {
        self.query_optional_i32_scalar(sea_orm::Statement::from_sql_and_values(
            self.conn.get_database_backend(),
            "SELECT COALESCE(MAX(sort_order), -1) FROM profiles",
            [],
        ))
        .await
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
        self.query_optional_i32_scalar(sea_orm::Statement::from_sql_and_values(
            self.conn.get_database_backend(),
            "SELECT COALESCE(MAX(sort_order), -1) FROM folders WHERE profile_id = ?",
            [profile_id.into()],
        ))
        .await
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
        let runtime = self.load_session_runtime_summary(id).await?;
        if runtime.is_active() {
            let reason = if runtime.state.is_active() {
                prefixed_text("runtime state is ", runtime.state.as_str())
            } else {
                "pending follow-up work exists".to_string()
            };
            let mut message = String::with_capacity(35 + id.len() + reason.len());
            message.push_str("cannot archive active session ");
            message.push_str(id);
            message.push_str(" while ");
            message.push_str(reason.as_str());
            return Err(ChatDbError::Db(DbErr::Custom(message)));
        }
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
        let mut active_session_ids = session_runtimes::Entity::find()
            .filter(session_runtimes::Column::State.ne(SessionRuntimeState::Idle.as_str()))
            .all(&self.conn)
            .await?
            .into_iter()
            .map(|row| row.session_id)
            .collect::<std::collections::BTreeSet<_>>();
        active_session_ids.extend(
            session_pending_messages::Entity::find()
                .all(&self.conn)
                .await?
                .into_iter()
                .map(|row| row.session_id),
        );
        active_session_ids.extend(
            session_pending_permissions::Entity::find()
                .all(&self.conn)
                .await?
                .into_iter()
                .map(|row| row.session_id),
        );
        active_session_ids.extend(
            session_pending_ask_user::Entity::find()
                .all(&self.conn)
                .await?
                .into_iter()
                .map(|row| row.session_id),
        );
        active_session_ids.extend(
            session_pending_workspace::Entity::find()
                .all(&self.conn)
                .await?
                .into_iter()
                .map(|row| row.session_id),
        );

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
        self.query_optional_i32_scalar(sea_orm::Statement::from_sql_and_values(
            self.conn.get_database_backend(),
            "SELECT COALESCE(MAX(pin_sort_order), -1) FROM sessions WHERE profile_id = ? AND pinned = 1",
            [profile_id.into()],
        ))
        .await
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
        status: TaskRunStatus,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let active = scheduled_tasks::ActiveModel {
            id: Set(id.to_string()),
            last_run_at: Set(Some(now.clone())),
            last_run_status: Set(Some(status.as_str().to_string())),
            last_run_output: Set(output.map(String::from)),
            updated_at: Set(now),
            ..Default::default()
        };
        active.update(&self.conn).await?;
        Ok(())
    }

    pub async fn start_task_run(
        &self,
        id: &str,
        task: &scheduled_tasks::Model,
        invocation_kind: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        scheduled_task_runs::ActiveModel {
            id: Set(id.to_string()),
            task_id: Set(task.id.clone()),
            profile_id: Set(task.profile_id.clone()),
            session_id: Set(task.session_id.clone()),
            invocation_kind: Set(invocation_kind.to_string()),
            status: Set(TaskRunStatus::Running.as_str().to_string()),
            output: Set(None),
            started_at: Set(now.clone()),
            finished_at: Set(None),
            updated_at: Set(now),
        }
        .insert(&self.conn)
        .await?;
        Ok(())
    }

    pub async fn finish_task_run(
        &self,
        id: &str,
        status: TaskRunStatus,
        output: Option<&str>,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        let mut active: scheduled_task_runs::ActiveModel =
            scheduled_task_runs::Entity::find_by_id(id)
                .one(&self.conn)
                .await?
                .ok_or_else(|| DbErr::RecordNotFound(id.to_string()))?
                .into();
        active.status = Set(status.as_str().to_string());
        active.output = Set(output.map(ToOwned::to_owned));
        active.finished_at = Set(Some(now.clone()));
        active.updated_at = Set(now);
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

    pub async fn list_tasks_for_webhook(
        &self,
        token: &str,
    ) -> Result<Vec<scheduled_tasks::Model>, ChatDbError> {
        scheduled_tasks::Entity::find()
            .filter(scheduled_tasks::Column::Enabled.eq(true))
            .filter(scheduled_tasks::Column::Approved.eq(true))
            .filter(scheduled_tasks::Column::TriggerKind.eq(TRIGGER_KIND_WEBHOOK))
            .filter(scheduled_tasks::Column::TriggerType.eq(token))
            .all(&self.conn)
            .await
            .map_err(Into::into)
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

    /// Mark a profile memory refresh job dirty.
    pub async fn mark_profile_memory_job_dirty(&self, profile_id: &str) -> Result<(), ChatDbError> {
        let updated_at = jiff::Timestamp::now().to_string();
        let active = profile_memory_jobs::ActiveModel {
            profile_id: Set(profile_id.to_string()),
            dirty: Set(true),
            last_processed_at: Set(None),
            updated_at: Set(updated_at),
        };
        profile_memory_jobs::Entity::insert(active)
            .on_conflict(
                sea_orm::sea_query::OnConflict::column(profile_memory_jobs::Column::ProfileId)
                    .update_columns([
                        profile_memory_jobs::Column::Dirty,
                        profile_memory_jobs::Column::UpdatedAt,
                    ])
                    .to_owned(),
            )
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    /// List profile memory jobs still marked dirty.
    pub async fn list_dirty_profile_memory_jobs(
        &self,
    ) -> Result<Vec<profile_memory_jobs::Model>, ChatDbError> {
        profile_memory_jobs::Entity::find()
            .filter(profile_memory_jobs::Column::Dirty.eq(true))
            .order_by_asc(profile_memory_jobs::Column::UpdatedAt)
            .all(&self.conn)
            .await
            .map_err(Into::into)
    }

    /// Mark a profile memory refresh job processed.
    pub async fn mark_profile_memory_job_processed(
        &self,
        profile_id: &str,
        expected_updated_at: &str,
    ) -> Result<(), ChatDbError> {
        let now = jiff::Timestamp::now().to_string();
        profile_memory_jobs::Entity::update_many()
            .col_expr(profile_memory_jobs::Column::Dirty, Expr::value(false))
            .col_expr(
                profile_memory_jobs::Column::LastProcessedAt,
                Expr::value(Some(now.clone())),
            )
            .col_expr(profile_memory_jobs::Column::UpdatedAt, Expr::value(now))
            .filter(profile_memory_jobs::Column::ProfileId.eq(profile_id))
            .filter(profile_memory_jobs::Column::UpdatedAt.eq(expected_updated_at))
            .exec(&self.conn)
            .await?;
        Ok(())
    }

    async fn query_optional_i32_scalar(
        &self,
        statement: sea_orm::Statement,
    ) -> Result<i32, ChatDbError> {
        let row: Option<sea_orm::QueryResult> = self.conn.query_one(statement).await?;
        Ok(row
            .and_then(|query_result| query_result.try_get_by_index::<i32>(0).ok())
            .unwrap_or(-1))
    }
}

fn query_u64(row: &sea_orm::QueryResult, index: usize) -> Result<u64, ChatDbError> {
    let value = row
        .try_get_by_index::<i64>(index)
        .map_err(ChatDbError::Db)?;
    u64::try_from(value).map_err(|error| {
        ChatDbError::Db(db_custom_error("u64 aggregate conversion failed: ", &error))
    })
}

/// Parameters for upserting a persisted session context snapshot.
pub struct SessionContextSnapshotUpsert<'a> {
    pub context_json: &'a str,
    pub assembled_messages_json: Option<&'a str>,
    pub metadata_json: Option<&'a str>,
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
    pub mode: PermissionHistoryMode,
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
    use sea_orm::EntityTrait;
    use tempfile::tempdir;

    use crate::PermissionRequestMode;
    use crate::db::entity::{scheduled_task_runs, session_run_leases};
    use crate::profile::ProfileEnvironment;
    use crate::session::{
        PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion,
        PersistedAskUserRequest, PersistedPermissionRequest, PersistedPermissionRequestPayload,
        PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, SessionRuntimeState,
    };

    use super::{ChatDb, CreateProfileParams, CreateScheduledTaskParams};

    fn test_pending_message(content: &str, mode: PendingUserMessageMode) -> PendingUserMessage {
        PendingUserMessage {
            content: content.to_string(),
            attachments: Vec::new(),
            hidden_context_xml: None,
            created_at: jiff::Timestamp::now().to_string(),
            delivery_mode: mode,
        }
    }

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
    async fn ensure_default_profiles_preserve_existing_profiles_by_slug() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_profile(CreateProfileParams {
            id: "daily-id",
            name: "Custom Daily",
            environment: "container",
            container_image: Some("custom-image"),
            slug: Some("daily"),
            icon: Some("D"),
            persona: None,
            preferences_json: None,
            location_aware: false,
        })
        .await
        .expect("daily profile should be created");
        db.create_profile(CreateProfileParams {
            id: "work-id",
            name: "Custom Work",
            environment: "container",
            container_image: Some("work-image"),
            slug: Some("work"),
            icon: Some("W"),
            persona: None,
            preferences_json: None,
            location_aware: true,
        })
        .await
        .expect("work profile should be created");
        db.create_profile(CreateProfileParams {
            id: "local-id",
            name: "Custom Local",
            environment: "local",
            container_image: Some("still-here"),
            slug: Some("local"),
            icon: Some("L"),
            persona: None,
            preferences_json: None,
            location_aware: true,
        })
        .await
        .expect("local profile should be created");

        let first_id = db
            .ensure_default_profiles()
            .await
            .expect("default profiles should exist");
        assert_eq!(first_id, "daily-id");

        let all = db.list_profiles().await.expect("profiles should be listed");
        assert_eq!(all.len(), 3, "existing slugged profiles should be reused");

        let daily = db
            .get_profile_by_slug("daily")
            .await
            .expect("daily query should succeed")
            .expect("daily profile should exist");
        assert_eq!(daily.id, "daily-id");
        assert_eq!(daily.name, "Custom Daily");
        assert_eq!(daily.environment, "container");
        assert_eq!(daily.icon.as_deref(), Some("D"));
        assert!(!daily.location_aware);

        let work = db
            .get_profile_by_slug("work")
            .await
            .expect("work query should succeed")
            .expect("work profile should exist");
        assert_eq!(work.id, "work-id");
        assert_eq!(work.name, "Custom Work");
        assert_eq!(work.environment, "container");
        assert_eq!(work.icon.as_deref(), Some("W"));
        assert!(work.location_aware);

        let local = db
            .get_profile_by_slug("local")
            .await
            .expect("local query should succeed")
            .expect("local profile should exist");
        assert_eq!(local.id, "local-id");
        assert_eq!(local.name, "Custom Local");
        assert_eq!(local.environment, "local");
        assert_eq!(local.icon.as_deref(), Some("L"));
        assert!(local.location_aware);
        assert!(local.container_image.is_none());
    }

    #[tokio::test]
    async fn preserves_older_steer_messages_when_new_steer_arrives() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");

        db.set_pending_immediate_message(
            session_id,
            test_pending_message("first steer", PendingUserMessageMode::Steer),
        )
        .await
        .expect("first steer should persist");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("second steer", PendingUserMessageMode::Steer),
        )
        .await
        .expect("second steer should persist");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(
            runtime
                .pending_immediate
                .as_ref()
                .map(|message| message.content.as_str()),
            Some("second steer")
        );
        assert_eq!(runtime.queued_messages.len(), 1);
        assert_eq!(runtime.queued_messages[0].content, "first steer");
        assert_eq!(
            runtime.queued_messages[0].delivery_mode,
            PendingUserMessageMode::Steer
        );

        let first = db
            .take_next_pending_message(session_id)
            .await
            .expect("first pending should load")
            .expect("first pending should exist");
        assert_eq!(first.content, "second steer");

        let second = db
            .take_next_pending_message(session_id)
            .await
            .expect("second pending should load")
            .expect("second pending should exist");
        assert_eq!(second.content, "first steer");
        assert_eq!(second.delivery_mode, PendingUserMessageMode::Steer);
    }

    #[tokio::test]
    async fn rejects_archiving_active_sessions() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_session_runtime_state(session_id, SessionRuntimeState::Running)
            .await
            .expect("runtime state should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("active session must not archive");
        assert!(
            error.to_string().contains("cannot archive active session"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn browser_takeover_state_persists_and_is_consumable() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_a = "session-a";
        let session_b = "session-b";
        db.create_session(session_a, Some("Session A"), Some("test-model"))
            .await
            .expect("first session should be created");
        db.create_session(session_b, Some("Session B"), Some("test-model"))
            .await
            .expect("second session should be created");

        db.set_browser_takeover_owner(session_a)
            .await
            .expect("browser takeover should persist");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("takeover owner should load"),
            Some(session_a.to_string())
        );
        assert!(
            db.session_has_browser_takeover(session_a)
                .await
                .expect("session takeover state should load")
        );

        db.clear_browser_takeover_owner(session_a)
            .await
            .expect("browser takeover release should persist");
        assert!(
            db.take_browser_takeover_alert_pending(session_a)
                .await
                .expect("pending takeover alert should be consumable")
        );
        assert!(
            !db.take_browser_takeover_alert_pending(session_a)
                .await
                .expect("pending takeover alert should clear after consumption")
        );

        db.set_browser_takeover_owner(session_b)
            .await
            .expect("browser takeover owner should switch");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("switched takeover owner should load"),
            Some(session_b.to_string())
        );
        assert!(
            !db.session_has_browser_takeover(session_a)
                .await
                .expect("previous takeover owner should be cleared")
        );

        db.recover_session_runtimes_after_restart()
            .await
            .expect("runtime recovery should clear stale takeover ownership");
        assert_eq!(
            db.find_browser_takeover_session()
                .await
                .expect("no active takeover should remain after restart"),
            None
        );
        assert!(
            db.take_browser_takeover_alert_pending(session_b)
                .await
                .expect("restart recovery should preserve takeover alert as pending")
        );
    }

    #[tokio::test]
    async fn request_logs_persist_in_database() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.append_request_log(super::RequestLogParams {
            session_id: "session-test",
            timestamp: "2026-03-14T00:00:00Z",
            model: "gpt-5",
            provider_id: "openai",
            prompt_tokens: Some(10),
            completion_tokens: Some(20),
            total_tokens: Some(30),
            cache_read_tokens: Some(5),
            cache_write_tokens: Some(0),
            context_window: Some(200000),
            elapsed_ms: 42,
            ok: false,
            error: Some("boom"),
        })
        .await
        .expect("request log should persist");

        let entries = db
            .list_request_logs(10)
            .await
            .expect("request logs should load");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "session-test");
        assert_eq!(entries[0].model, "gpt-5");
        assert_eq!(entries[0].provider_id, "openai");
        assert_eq!(entries[0].prompt_tokens, Some(10));
        assert_eq!(entries[0].completion_tokens, Some(20));
        assert_eq!(entries[0].total_tokens, Some(30));
        assert_eq!(entries[0].cache_read_tokens, Some(5));
        assert_eq!(entries[0].context_window, Some(200000));
        assert_eq!(entries[0].elapsed_ms, 42);
        assert!(!entries[0].ok);
        assert_eq!(entries[0].error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn rejects_archiving_sessions_with_pending_follow_up_work() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("steer me", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("session with pending work must not archive");
        assert!(
            error.to_string().contains("pending follow-up work exists"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn rejects_archiving_sessions_waiting_on_workspace_access() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_waiting_workspace(
            session_id,
            Some(PersistedWorkspaceRequest {
                id: "workspace-1".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "Need write access".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting workspace should persist");

        let error = db
            .archive_session(session_id)
            .await
            .expect_err("session with pending workspace request must not archive");
        assert!(
            error.to_string().contains("cannot archive active session"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn creates_remote_profile_with_typed_environment() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "remote-profile",
                name: "Remote",
                environment: "remote",
                container_image: None,
                slug: None,
                icon: None,
                persona: None,
                preferences_json: Some(r#"{"remote":{"ssh_server_id":"prod"}}"#),
                location_aware: false,
            })
            .await
            .expect("remote profile should be created");

        let info = crate::profile::ProfileInfo::from_db_model(model)
            .expect("remote profile should deserialize");
        match info.environment {
            ProfileEnvironment::Remote { ssh_server_id } => assert_eq!(ssh_server_id, "prod"),
            other => panic!("expected remote environment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_remote_profile_without_ssh_server_binding() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let model = db
            .create_profile(CreateProfileParams {
                id: "remote-profile",
                name: "Remote",
                environment: "remote",
                container_image: None,
                slug: None,
                icon: None,
                persona: None,
                preferences_json: Some(r#"{"remote":{"ssh_server_id":"  "}}"#),
                location_aware: false,
            })
            .await
            .expect("remote profile row should be stored");

        let error = crate::profile::ProfileInfo::from_db_model(model)
            .expect_err("invalid remote preferences must fail fast");
        assert!(
            error
                .to_string()
                .contains("preferences.remote.ssh_server_id"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn restart_recovery_preserves_waiting_requests_and_pending_dispatch() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("steer me", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");
        db.set_waiting_permission(
            session_id,
            Some(PersistedPermissionRequest {
                id: "perm-1".to_string(),
                request: PersistedPermissionRequestPayload {
                    mode: PermissionRequestMode::Default,
                    script: "echo hi".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting permission should persist");
        db.set_waiting_ask_user(
            session_id,
            Some(PersistedAskUserRequest {
                id: "ask-1".to_string(),
                questions: vec![PersistedAskUserQuestion {
                    section: "default".to_string(),
                    question: "Proceed?".to_string(),
                    option: vec!["yes".to_string()],
                    multi_select: false,
                }],
            }),
        )
        .await
        .expect("waiting ask-user should persist");
        db.set_waiting_workspace(
            session_id,
            Some(PersistedWorkspaceRequest {
                id: "workspace-1".to_string(),
                request: PersistedWorkspaceRequestPayload {
                    path: "/tmp/project".to_string(),
                    reason: "Need write access".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting workspace should persist");

        db.recover_session_runtimes_after_restart()
            .await
            .expect("runtime recovery should succeed");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(runtime.state, SessionRuntimeState::WaitingPermission);
        assert!(runtime.waiting_permission.is_some());
        assert!(runtime.waiting_workspace.is_some());
        assert!(runtime.waiting_ask_user.is_some());
        assert!(runtime.has_pending_immediate());
        assert_eq!(runtime.queued_count, 1);

        let resumable = db
            .sessions_requiring_pending_dispatch()
            .await
            .expect("pending dispatch lookup should succeed");
        assert!(resumable.is_empty());
    }

    #[tokio::test]
    async fn restart_recovery_expires_terminal_wait_state() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_session_runtime_state(session_id, SessionRuntimeState::WaitingTerminalInput)
            .await
            .expect("waiting terminal input should persist");

        db.recover_session_runtimes_after_restart()
            .await
            .expect("runtime recovery should succeed");

        let runtime = db
            .get_session_runtime(session_id)
            .await
            .expect("runtime should load")
            .expect("runtime should exist");
        assert_eq!(runtime.state, SessionRuntimeState::Idle);
        assert!(!runtime.has_pending_messages());
    }

    #[tokio::test]
    async fn dispatchable_pending_message_requires_idle_runtime() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let session_id = "session-test";
        db.create_session(session_id, Some("Test session"), Some("test-model"))
            .await
            .expect("session should be created");
        db.set_pending_immediate_message(
            session_id,
            test_pending_message("resume later", PendingUserMessageMode::Steer),
        )
        .await
        .expect("pending steer should persist");
        db.set_waiting_permission(
            session_id,
            Some(PersistedPermissionRequest {
                id: "perm-1".to_string(),
                request: PersistedPermissionRequestPayload {
                    mode: PermissionRequestMode::Default,
                    script: "echo hi".to_string(),
                    session_id: Some(session_id.to_string()),
                },
            }),
        )
        .await
        .expect("waiting permission should persist");

        let blocked = db
            .take_next_dispatchable_pending_message(session_id)
            .await
            .expect("dispatchable lookup should succeed");
        assert!(blocked.is_none());

        db.set_waiting_permission(session_id, None)
            .await
            .expect("waiting permission should clear");
        db.set_session_runtime_state(session_id, SessionRuntimeState::Idle)
            .await
            .expect("runtime should become idle before resumed dispatch");

        let resumed = db
            .take_next_dispatchable_pending_message(session_id)
            .await
            .expect("dispatchable lookup should succeed")
            .expect("pending steer should dispatch once idle");
        assert_eq!(resumed.content, "resume later");
    }

    #[tokio::test]
    async fn notification_callback_consumes_only_once() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        let payload = crate::notification::NotificationPayload {
            notification_id: Some("notification-1".to_string()),
            title: "Task".to_string(),
            body: "Completed".to_string(),
            session_id: Some("session-1".to_string()),
            callback_context_xml: Some(
                crate::XmlFragment::parse("<system_alert><status>ok</status></system_alert>")
                    .expect("callback XML should parse"),
            ),
            callback_message: Some("Follow up".to_string()),
        };
        db.create_notification_mailbox_entry("notification-1", &payload, "delivered")
            .await
            .expect("mailbox entry should persist");

        let first = db
            .consume_notification_callback("notification-1")
            .await
            .expect("first callback consume should succeed");
        assert!(first.has_callback);
        assert!(first.callback_consumed);
        assert_eq!(first.callback_message.as_deref(), Some("Follow up"));

        let second = db
            .consume_notification_callback("notification-1")
            .await
            .expect("second callback consume should succeed");
        assert!(second.has_callback);
        assert!(!second.callback_consumed);
        assert_eq!(second.callback_message.as_deref(), Some("Follow up"));
    }

    #[tokio::test]
    async fn task_run_recovery_marks_running_runs_interrupted() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_scheduled_task(CreateScheduledTaskParams {
            id: "task-1",
            profile_id: "profile-1",
            session_id: Some("session-1"),
            name: "Demo task",
            trigger_kind: "event",
            trigger_type: "chat.turn.completed",
            cron_expression: None,
            run_at: None,
            script_path: None,
            script_inline: Some("echo hi"),
            needs_network: false,
        })
        .await
        .expect("task should persist");
        let task = db
            .get_scheduled_task("task-1")
            .await
            .expect("task lookup should succeed")
            .expect("task should exist");
        db.start_task_run("run-1", &task, "event")
            .await
            .expect("task run should persist");

        db.recover_task_runs_after_restart()
            .await
            .expect("task run recovery should succeed");

        let recovered = scheduled_task_runs::Entity::find_by_id("run-1")
            .one(&db.conn)
            .await
            .expect("task run lookup should succeed")
            .expect("task run should exist");
        assert_eq!(recovered.status, "interrupted");
        assert!(recovered.finished_at.is_some());
        assert_eq!(
            recovered.output.as_deref(),
            Some("May server restarted before the task run completed")
        );
    }

    #[tokio::test]
    async fn session_run_lease_rejects_second_claim_until_released() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_session("session-1", Some("Session"), Some("test-model"))
            .await
            .expect("session should persist");

        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("first lease claim should succeed")
        );
        assert!(
            !db.claim_session_run_lease("session-1")
                .await
                .expect("second lease claim should be rejected")
        );
        assert!(
            db.request_session_run_cancel("session-1")
                .await
                .expect("cancellation request should persist against active lease")
        );

        db.release_session_run_lease("session-1")
            .await
            .expect("lease release should persist");

        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("released lease should become claimable again")
        );
    }

    #[tokio::test]
    async fn session_run_lease_recovery_clears_stale_active_run() {
        let dir = tempdir().expect("temp dir should be created");
        let db_path = dir.path().join("history.db");
        let db = ChatDb::open(&db_path)
            .await
            .expect("db should initialize successfully");

        db.create_session("session-1", Some("Session"), Some("test-model"))
            .await
            .expect("session should persist");
        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("lease claim should succeed")
        );
        assert!(
            db.request_session_run_cancel("session-1")
                .await
                .expect("cancellation request should persist")
        );

        db.recover_session_run_leases_after_restart()
            .await
            .expect("lease recovery should clear stale active state");

        let recovered = session_run_leases::Entity::find_by_id("session-1")
            .one(&db.conn)
            .await
            .expect("lease lookup should succeed")
            .expect("lease row should exist");
        assert!(!recovered.active);
        assert!(!recovered.cancellation_requested);
        assert!(
            db.claim_session_run_lease("session-1")
                .await
                .expect("recovered lease should become claimable")
        );
    }
}
