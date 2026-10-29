//! Shared types, error enums, row structs, and helper functions for the
//! `ChatDb` layer.
//!
//! Everything here is serde-deserialized row shapes, parameter bundles,
//! and small parse/build helpers. The [`ChatDb`](super::ChatDb) struct
//! itself and its impl block live in `super::mod.rs` so that every
//! method has direct access to the private `db: SkyDb` field without
//! visibility gymnastics.

use skyzen_services::DbError as SkyDbError;

use crate::profile::{ContainerMountPreference, ContainerProfilePreferences, ProfilePreferences};
use crate::session::{ChatMessageStats, SessionPlanState, SessionRuntimeState};

use super::entity::profiles;

/// Error type for database operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatDbError {
    /// Underlying store failure; carries the driver error message.
    #[error("Database backend error: {0}")]
    Storage(String),
    /// The queried row does not exist; the string identifies what was looked up.
    #[error("Database row not found: {0}")]
    RowNotFound(String),
    /// A persisted context fragment failed XML block validation.
    #[error("Invalid context fragment: {0}")]
    InvalidXml(#[from] crate::xml::XmlBlockError),
    /// A profile row carries an environment value that is not recognized or no longer allowed.
    #[error("Invalid profile environment: {0}")]
    InvalidProfileEnvironment(String),
    /// A run-scoped write was refused because the run's lease was taken over
    /// or released.
    #[error("The run lease of session {session_id} is no longer held by this run")]
    RunLeaseLost {
        /// Session whose lease the run no longer holds.
        session_id: String,
    },
    /// A pin operation would exceed the per-profile pinned-item limit.
    #[error(
        "Pinned limit exceeded for profile {profile_id}: current pinned items {current}, limit {limit}"
    )]
    PinnedLimitExceeded {
        /// Profile whose pin list hit the limit.
        profile_id: String,
        /// Configured maximum number of pinned items.
        limit: i64,
        /// Number of items already pinned.
        current: i64,
    },
}

impl From<SkyDbError> for ChatDbError {
    fn from(error: SkyDbError) -> Self {
        match error {
            SkyDbError::RowNotFound => Self::RowNotFound("database row not found".to_string()),
            other => Self::Storage(other.to_string()),
        }
    }
}

/// Parameters for creating a new profile.
#[derive(Debug)]
pub struct CreateProfileParams<'a> {
    /// Id to create the profile under.
    pub id: &'a str,
    /// Display name.
    pub name: &'a str,
    /// Environment the profile's agents run in.
    pub environment: ProfileEnvironmentKind,
    /// Container image reference; only meaningful for `Container` environments.
    pub container_image: Option<&'a str>,
    /// Optional friendly slug; a default is derived when absent.
    pub slug: Option<&'a str>,
    /// Optional icon identifier for pickers.
    pub icon: Option<&'a str>,
    /// Optional persona text folded into the system prompt.
    pub persona: Option<&'a str>,
    /// Optional initial preferences (mounts, dockerfile, ..).
    pub preferences: Option<&'a crate::profile::ProfilePreferences>,
    /// Whether the profile may use device location context.
    pub location_aware: bool,
}

/// Parameters for updating a profile's mutable fields.
#[derive(Debug)]
pub struct UpdateProfileParams<'a> {
    /// New display name, if changing.
    pub name: Option<&'a str>,
    /// Icon update; `Some(None)` clears the stored icon.
    pub icon: Option<Option<&'a str>>,
    /// Environment update.
    pub environment: Option<ProfileEnvironmentKind>,
    /// Container image update; `Some(None)` clears it.
    pub container_image: Option<Option<&'a str>>,
    /// Attached container id update; `Some(None)` detaches.
    pub container_id: Option<Option<&'a str>>,
    /// Sort position update.
    pub sort_order: Option<i32>,
    /// Persona update; `Some(None)` clears it.
    pub persona: Option<Option<&'a str>>,
    /// Preferences update; `Some(None)` clears stored preferences.
    pub preferences: Option<Option<&'a crate::profile::ProfilePreferences>>,
    /// Location-awareness toggle update.
    pub location_aware: Option<bool>,
}

/// Persisted assistant usage/context stats.
#[derive(Debug, Clone, Default)]
pub struct MessageStatsParams {
    /// Provider that produced the message.
    pub provider_id: Option<String>,
    /// Prompt (input) token count.
    pub prompt_tokens: Option<u32>,
    /// Completion (output) token count.
    pub completion_tokens: Option<u32>,
    /// Total tokens billed for the message.
    pub total_tokens: Option<u32>,
    /// Tokens served from the provider's prompt cache.
    pub cache_read_tokens: Option<u32>,
    /// Tokens written into the provider's prompt cache.
    pub cache_write_tokens: Option<u32>,
    /// Model context window size at request time.
    pub context_window: Option<u32>,
    /// Wall-clock time the request took, in milliseconds.
    pub elapsed_ms: Option<u64>,
}

/// Row payload recording one network access decision.
#[derive(Debug, Clone)]
pub struct NetworkAccessLogParams<'a> {
    /// Session the access was attributed to, if any.
    pub session_id: Option<&'a str>,
    /// Profile the access was attributed to, if any.
    pub profile_id: Option<&'a str>,
    /// Which surface attempted the access.
    pub source: NetworkAccessSource,
    /// Access mode (currently only `network`).
    pub mode: NetworkAccessMode,
    /// Target host.
    pub domain: &'a str,
    /// Target port.
    pub port: u16,
    /// Whether policy allowed the access.
    pub allowed: bool,
    /// Optional human-readable policy reason.
    pub reason: Option<&'a str>,
}

/// Which subsystem attempted a network access, persisted as `snake_case`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccessSource {
    /// Interactive chat agent.
    Chat,
    /// `may exec` one-shot command execution.
    Exec,
    /// Scheduled task running on the host.
    TaskLocal,
    /// Scheduled-task validation run on the host.
    TaskValidationLocal,
    /// Scheduled task on a remote target.
    TaskRemote,
    /// Scheduled task inside a container.
    TaskContainer,
    /// Scheduled-task validation inside a container.
    TaskValidationContainer,
}

impl NetworkAccessSource {
    /// Persisted lowercase string form used in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Exec => "exec",
            Self::TaskLocal => "task_local",
            Self::TaskValidationLocal => "task_validation_local",
            Self::TaskRemote => "task_remote",
            Self::TaskContainer => "task_container",
            Self::TaskValidationContainer => "task_validation_container",
        }
    }
}

impl std::fmt::Display for NetworkAccessSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for NetworkAccessSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "chat" => Ok(Self::Chat),
            "exec" => Ok(Self::Exec),
            "task_local" => Ok(Self::TaskLocal),
            "task_validation_local" => Ok(Self::TaskValidationLocal),
            "task_remote" => Ok(Self::TaskRemote),
            "task_container" => Ok(Self::TaskContainer),
            "task_validation_container" => Ok(Self::TaskValidationContainer),
            other => Err(format!("unknown network access source: {other}")),
        }
    }
}

/// Network access mode stored for a log row.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccessMode {
    /// Plain outbound network access.
    Network,
}

impl NetworkAccessMode {
    /// Persisted lowercase string form used in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
        }
    }
}

impl std::fmt::Display for NetworkAccessMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for NetworkAccessMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "network" => Ok(Self::Network),
            other => Err(format!("unknown network access mode: {other}")),
        }
    }
}

/// Row payload for one provider request log entry.
#[derive(Debug, Clone)]
pub struct RequestLogParams<'a> {
    /// Session the request belongs to.
    pub session_id: &'a str,
    /// Request timestamp as an ISO string.
    pub timestamp: &'a str,
    /// Model id the request was sent to.
    pub model: &'a str,
    /// Provider that served the request.
    pub provider_id: &'a str,
    /// Prompt (input) token count.
    pub prompt_tokens: Option<u32>,
    /// Completion (output) token count.
    pub completion_tokens: Option<u32>,
    /// Total tokens billed for the request.
    pub total_tokens: Option<u32>,
    /// Tokens served from the provider's prompt cache.
    pub cache_read_tokens: Option<u32>,
    /// Tokens written into the provider's prompt cache.
    pub cache_write_tokens: Option<u32>,
    /// Model context window size at request time.
    pub context_window: Option<u32>,
    /// Wall-clock time the request took, in milliseconds.
    pub elapsed_ms: u64,
    /// Whether the request completed successfully.
    pub ok: bool,
    /// Error summary when `ok` is false.
    pub error: Option<&'a str>,
}

/// Aggregate prompt-cache statistics across assistant messages.
#[derive(Debug, Clone, Default)]
pub struct AssistantCacheUsageSummary {
    /// Total assistant messages counted.
    pub assistant_message_count: u64,
    /// Messages that read from the provider cache.
    pub cache_hit_message_count: u64,
    /// Summed prompt tokens.
    pub prompt_tokens_total: u64,
    /// Summed cache-read tokens.
    pub cache_read_tokens_total: u64,
    /// Summed cache-write tokens.
    pub cache_write_tokens_total: u64,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PendingPermissionLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PendingAskUserLookup {
    pub(crate) session_id: String,
    /// Whether a run is waiting on the prompt, as opposed to an open question.
    pub(crate) blocking: bool,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PendingWorkspaceLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingMessageEntryRow {
    pub(super) id: i64,
    pub(super) delivery_mode: String,
    pub(super) content: String,
    pub(super) hidden_context_xml: Option<String>,
    pub(super) created_at: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingMessageAttachmentRow {
    pub(super) message_id: i64,
    pub(super) attachment_id: String,
    pub(super) name: String,
    pub(super) attachment_type: String,
    pub(super) url: String,
    pub(super) upload_data_url: Option<String>,
    pub(super) text_content: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingAskUserRequestRow {
    pub(super) request_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingAskUserQuestionRow {
    pub(super) ordinal: i64,
    pub(super) section: String,
    pub(super) question: String,
    pub(super) multi_select: bool,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingAskUserQuestionOptionRow {
    pub(super) question_ordinal: i64,
    pub(super) value: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ProfileMountRow {
    pub(super) host_path: String,
    pub(super) container_path: String,
    pub(super) access: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PermissionRequestMessageRow {
    pub(super) request_id: String,
    pub(super) mode: String,
    pub(super) script: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AskUserRequestMessageRow {
    pub(super) message_id: i32,
    pub(super) request_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AskUserRequestQuestionRow {
    pub(super) message_id: i32,
    pub(super) ordinal: i64,
    pub(super) section: String,
    pub(super) question: String,
    pub(super) multi_select: bool,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AskUserRequestQuestionOptionRow {
    pub(super) message_id: i32,
    pub(super) question_ordinal: i64,
    pub(super) value: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugSnapshotHeaderRow {
    pub(super) session_id: String,
    pub(super) timestamp: String,
    pub(super) model: String,
    pub(super) provider_id: String,
    pub(super) prompt: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugPromptAttachmentRow {
    pub(super) url: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugMessageRow {
    pub(super) ordinal: i64,
    pub(super) role: String,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugMessageAttachmentRow {
    pub(super) message_ordinal: i64,
    pub(super) url: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugMessageToolCallRow {
    pub(super) message_ordinal: i64,
    pub(super) name: String,
}

/// Materialized view of a persisted session context snapshot, for debugging.
#[derive(Debug, Clone)]
pub struct ContextDebugSnapshot {
    /// Session the snapshot belongs to.
    pub session_id: String,
    /// Snapshot timestamp as an ISO string.
    pub timestamp: String,
    /// Model id recorded in the snapshot header.
    pub model: String,
    /// Provider id recorded in the snapshot header.
    pub provider_id: String,
    /// User prompt captured at snapshot time.
    pub prompt: String,
    /// Attachment URLs included with the prompt.
    pub prompt_attachments: Vec<String>,
    /// Messages assembled into the request context, in order.
    pub assembled_messages: Vec<ContextDebugSnapshotMessage>,
}

/// One message inside a [`ContextDebugSnapshot`].
#[derive(Debug, Clone)]
pub struct ContextDebugSnapshotMessage {
    /// Message role (`user`, `assistant`, ..).
    pub role: String,
    /// Message body text.
    pub content: String,
    /// Attachment URLs carried by the message.
    pub attachments: Vec<String>,
    /// Tool call id when the message answers one.
    pub tool_call_id: Option<String>,
    /// Names of tool calls the message issued.
    pub tool_call_names: Vec<String>,
}

/// Outcome of consuming a notification's pending callback.
#[derive(Debug, Clone)]
pub struct NotificationCallbackConsumeResult {
    /// Notification id that was consumed.
    pub id: String,
    /// Session the notification belongs to, if any.
    pub session_id: Option<String>,
    /// Profile the notification belongs to, if any.
    pub profile_id: Option<String>,
    /// Serialized context fragment attached to the callback.
    pub callback_context_xml: Option<String>,
    /// Callback message text delivered to the session.
    pub callback_message: Option<String>,
    /// Whether a callback was registered for the notification.
    pub has_callback: bool,
    /// Whether the callback had already been consumed.
    pub callback_consumed: bool,
}

pub(super) struct DefaultProfile<'a> {
    pub(super) slug: &'static str,
    pub(super) name: &'a str,
    pub(super) icon: &'static str,
    pub(super) environment: &'static str,
    pub(super) location_aware: bool,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct SqliteTableInfoRow {
    pub(super) name: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct I32ValueRow {
    pub(super) value: i32,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct StringValueRow {
    pub(super) value: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct I64ValueRow {
    pub(super) value: i64,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct OptionalI64ValueRow {
    pub value: Option<i64>,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AssistantCacheUsageSummaryRow {
    pub(super) assistant_message_count: i64,
    pub(super) cache_hit_message_count: i64,
    pub(super) prompt_tokens_total: i64,
    pub(super) cache_read_tokens_total: i64,
    pub(super) cache_write_tokens_total: i64,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct SessionIdRow {
    pub(super) session_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct StoredSessionContextSnapshotRow {
    pub(super) format_version: i64,
    pub(super) checkpoint_blob: Vec<u8>,
    pub(super) last_message_db_id: Option<i64>,
    pub(super) updated_at: String,
}

/// Parameters for upserting a persisted session context snapshot.
#[derive(Debug)]
pub struct SessionContextSnapshotUpsert {
    /// Serialized agent checkpoint to persist.
    pub checkpoint: aither_agent::AgentCheckpoint,
    /// The DB `messages.id` of the last message at snapshot time.
    /// `None` means the snapshot was taken with an empty history.
    pub last_message_db_id: Option<i64>,
}

/// Structured snapshot loaded from SQL-backed session context storage.
#[derive(Debug, Clone)]
pub struct StoredSessionContextSnapshot {
    /// Deserialized agent checkpoint.
    pub checkpoint: aither_agent::AgentCheckpoint,
    /// The DB `messages.id` of the last message at snapshot time.
    pub last_message_db_id: Option<i64>,
    /// Last update timestamp as an ISO string.
    pub updated_at: String,
}

/// Parameters for creating a scheduled task.
#[derive(Debug)]
pub struct CreateScheduledTaskParams<'a> {
    /// Id to create the task under.
    pub id: &'a str,
    /// Profile the task runs under.
    pub profile_id: &'a str,
    /// Session the task is bound to, if any.
    pub session_id: Option<&'a str>,
    /// Human-readable task name.
    pub name: &'a str,
    /// Trigger definition (time, event, or webhook).
    pub trigger: &'a crate::task_trigger::PersistedTaskTrigger,
    /// Cron expression for recurring tasks.
    pub cron_expression: Option<&'a str>,
    /// ISO timestamp for one-shot tasks.
    pub run_at: Option<&'a str>,
    /// Path to a script file the task runs.
    pub script_path: Option<&'a str>,
    /// Inline script body the task runs.
    pub script_inline: Option<&'a str>,
    /// Whether the task requires outbound network access.
    pub needs_network: bool,
    /// Maximum runtime of one run in seconds.
    pub timeout_secs: u32,
}

/// Parameters for updating a scheduled task.
#[derive(Debug)]
pub struct UpdateScheduledTaskParams<'a> {
    /// New task name, if changing.
    pub name: Option<&'a str>,
    /// Enabled flag update.
    pub enabled: Option<bool>,
    /// Approval flag update.
    pub approved: Option<bool>,
    /// Cron schedule update; `Some(None)` clears it.
    pub cron_expression: Option<Option<&'a str>>,
    /// One-shot timestamp update; `Some(None)` clears it.
    pub run_at: Option<Option<&'a str>>,
    /// Script path update; `Some(None)` clears it.
    pub script_path: Option<Option<&'a str>>,
    /// Inline script update; `Some(None)` clears it.
    pub script_inline: Option<Option<&'a str>>,
    /// Run timeout update in seconds.
    pub timeout_secs: Option<u32>,
}

/// Where a profile's agent workload runs, persisted as lowercase.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ProfileEnvironmentKind {
    /// Runs on the host.
    Local,
    /// Runs inside a container.
    Container,
}

impl ProfileEnvironmentKind {
    /// Lowercase persisted/serde form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Container => "container",
        }
    }
}

impl std::fmt::Display for ProfileEnvironmentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for ProfileEnvironmentKind {
    type Err = ChatDbError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_profile_environment(value)
    }
}

pub(super) fn parse_profile_environment(
    value: &str,
) -> Result<ProfileEnvironmentKind, ChatDbError> {
    match value {
        "local" => Ok(ProfileEnvironmentKind::Local),
        "container" => Ok(ProfileEnvironmentKind::Container),
        "remote" => Err(ChatDbError::InvalidProfileEnvironment(
            "remote is no longer a valid home environment; legacy rows should have been migrated to local"
                .to_string(),
        )),
        other => Err(ChatDbError::InvalidProfileEnvironment(other.to_string())),
    }
}

pub(super) fn db_custom_error(prefix: &str, error: &impl std::fmt::Display) -> String {
    format!("{prefix}{error}")
}

pub(super) fn session_not_found_db_error(session_id: &str) -> ChatDbError {
    ChatDbError::RowNotFound(format!("Session not found: {session_id}"))
}

pub(super) fn row_not_found(message: impl Into<String>) -> ChatDbError {
    ChatDbError::RowNotFound(message.into())
}

pub(super) fn storage_error(message: impl Into<String>) -> ChatDbError {
    ChatDbError::Storage(message.into())
}

pub(super) fn conversion_error(field: &str, error: &impl std::fmt::Display) -> ChatDbError {
    storage_error(format!("{field} conversion failed: {error}"))
}

pub(super) fn overflow_error(field: &str, error: &impl std::fmt::Display) -> ChatDbError {
    storage_error(format!("{field} overflow: {error}"))
}

pub(super) fn to_i32(value: u32, field: &str) -> Result<i32, ChatDbError> {
    i32::try_from(value).map_err(|error| overflow_error(field, &error))
}

pub(super) fn to_i64_from_u64(value: u64, field: &str) -> Result<i64, ChatDbError> {
    i64::try_from(value).map_err(|error| overflow_error(field, &error))
}

pub(super) fn to_usize(value: i64, field: &str) -> Result<usize, ChatDbError> {
    usize::try_from(value).map_err(|error| conversion_error(field, &error))
}

pub(super) fn to_u64(value: i64, field: &str) -> Result<u64, ChatDbError> {
    u64::try_from(value).map_err(|error| conversion_error(field, &error))
}

pub(super) fn message_stats_params_from_chat_stats(stats: &ChatMessageStats) -> MessageStatsParams {
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

pub(super) fn parse_pending_user_message_mode(
    value: &str,
) -> Result<crate::session::PendingUserMessageMode, ChatDbError> {
    value
        .parse()
        .map_err(|error| storage_error(format!("invalid pending message mode: {error}")))
}

pub(super) fn parse_session_runtime_state(value: &str) -> Result<SessionRuntimeState, ChatDbError> {
    value
        .parse()
        .map_err(|error| storage_error(format!("invalid session runtime state: {error}")))
}

pub(super) fn parse_session_plan_state(value: &str) -> Result<SessionPlanState, ChatDbError> {
    value
        .parse()
        .map_err(|error| storage_error(format!("invalid session plan state: {error}")))
}

pub(super) fn parse_attachment_kind(
    value: &str,
) -> Result<crate::session::AttachmentKind, ChatDbError> {
    match value {
        "image" => Ok(crate::session::AttachmentKind::Image),
        "file" => Ok(crate::session::AttachmentKind::File),
        "text" => Ok(crate::session::AttachmentKind::Text),
        "document" => Ok(crate::session::AttachmentKind::Document),
        "pdf" => Ok(crate::session::AttachmentKind::Pdf),
        "audio" => Ok(crate::session::AttachmentKind::Audio),
        "video" => Ok(crate::session::AttachmentKind::Video),
        other => Err(storage_error(format!("invalid attachment kind: {other}"))),
    }
}

pub(super) fn normalize_profile_preferences(
    environment: ProfileEnvironmentKind,
    preferences: Option<&ProfilePreferences>,
) -> Option<ProfilePreferences> {
    let mut preferences = preferences.cloned().unwrap_or_default();
    match environment {
        ProfileEnvironmentKind::Local => {
            preferences.container = None;
        }
        ProfileEnvironmentKind::Container => {}
    }
    preferences.container.is_some().then_some(preferences)
}

pub(super) fn build_profile_preferences(
    model: &profiles::Model,
    mounts: Vec<ContainerMountPreference>,
) -> Option<ProfilePreferences> {
    let container = if model.container_dockerfile.is_some() || !mounts.is_empty() {
        Some(ContainerProfilePreferences {
            dockerfile: model.container_dockerfile.clone(),
            mounts,
        })
    } else {
        None
    };
    container
        .is_some()
        .then_some(ProfilePreferences { container })
}
