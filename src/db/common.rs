//! Shared types, error enums, row structs, and helper functions for the
//! `ChatDb` layer.
//!
//! Everything here is serde-deserialized row shapes, parameter bundles,
//! and small parse/build helpers. The [`ChatDb`](super::ChatDb) struct
//! itself and its impl block live in `super::mod.rs` so that every
//! method has direct access to the private `db: SkyDb` field without
//! visibility gymnastics.

use skyzen_services::DbError as SkyDbError;

use crate::ask_user::AnswerValue;
use crate::profile::{ContainerMountPreference, ContainerProfilePreferences, ProfilePreferences};
use crate::session::{ChatMessageStats, SessionPlanState, SessionRuntimeState};

use super::entity::profiles;

/// Error type for database operations.
#[derive(Debug, thiserror::Error)]
pub enum ChatDbError {
    #[error("Database backend error: {0}")]
    Storage(String),
    #[error("Database row not found: {0}")]
    RowNotFound(String),
    #[error("Invalid context fragment: {0}")]
    InvalidXml(#[from] crate::xml::XmlBlockError),
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

impl From<SkyDbError> for ChatDbError {
    fn from(error: SkyDbError) -> Self {
        match error {
            SkyDbError::RowNotFound => Self::RowNotFound("database row not found".to_string()),
            other => Self::Storage(other.to_string()),
        }
    }
}

/// Parameters for creating a new profile.
pub struct CreateProfileParams<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub environment: ProfileEnvironmentKind,
    pub container_image: Option<&'a str>,
    pub slug: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub persona: Option<&'a str>,
    pub preferences: Option<&'a crate::profile::ProfilePreferences>,
    pub location_aware: bool,
}

/// Parameters for updating a profile's mutable fields.
pub struct UpdateProfileParams<'a> {
    pub name: Option<&'a str>,
    pub icon: Option<Option<&'a str>>,
    pub environment: Option<ProfileEnvironmentKind>,
    pub container_image: Option<Option<&'a str>>,
    pub container_id: Option<Option<&'a str>>,
    pub sort_order: Option<i32>,
    pub persona: Option<Option<&'a str>>,
    pub preferences: Option<Option<&'a crate::profile::ProfilePreferences>>,
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
    pub source: NetworkAccessSource,
    pub mode: NetworkAccessMode,
    pub domain: &'a str,
    pub port: u16,
    pub allowed: bool,
    pub reason: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccessSource {
    Chat,
    Exec,
    TaskLocal,
    TaskValidationLocal,
    TaskRemote,
    TaskContainer,
    TaskValidationContainer,
}

impl NetworkAccessSource {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetworkAccessMode {
    Network,
}

impl NetworkAccessMode {
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

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PendingPermissionLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PendingAskUserLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub struct PendingWorkspaceLookup {
    pub(crate) session_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingMessageEntryRow {
    pub(super) id: i64,
    pub(super) session_id: String,
    pub(super) delivery_mode: String,
    pub(super) content: String,
    pub(super) hidden_context_xml: Option<String>,
    pub(super) created_at: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingMessageAttachmentRow {
    pub(super) message_id: i64,
    pub(super) ordinal: i64,
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
    pub(super) session_id: String,
    pub(super) created_at: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingAskUserQuestionRow {
    pub(super) request_id: String,
    pub(super) ordinal: i64,
    pub(super) section: String,
    pub(super) question: String,
    pub(super) multi_select: bool,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct PendingAskUserQuestionOptionRow {
    pub(super) request_id: String,
    pub(super) question_ordinal: i64,
    pub(super) option_ordinal: i64,
    pub(super) value: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ProfileMountRow {
    pub(super) profile_id: String,
    pub(super) ordinal: i64,
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
pub(super) struct PermissionResponseMessageRow {
    pub(super) request_id: String,
    pub(super) approved: bool,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct WorkspaceRequestMessageRow {
    pub(super) request_id: String,
    pub(super) path: String,
    pub(super) reason: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct WorkspaceResponseMessageRow {
    pub(super) request_id: String,
    pub(super) approved: bool,
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
    pub(super) option_ordinal: i64,
    pub(super) value: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AskUserResponseMessageRow {
    pub(super) message_id: i32,
    pub(super) request_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AskUserResponseAnswerRow {
    pub(super) message_id: i32,
    pub(super) ordinal: i64,
    pub(super) section: String,
    pub(super) answer_kind: String,
    pub(super) custom_text: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct AskUserResponseSelectedOptionRow {
    pub(super) message_id: i32,
    pub(super) answer_ordinal: i64,
    pub(super) option_ordinal: i64,
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
    pub(super) session_id: String,
    pub(super) ordinal: i64,
    pub(super) url: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugMessageRow {
    pub(super) session_id: String,
    pub(super) ordinal: i64,
    pub(super) role: String,
    pub(super) content: String,
    pub(super) tool_call_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugMessageAttachmentRow {
    pub(super) session_id: String,
    pub(super) message_ordinal: i64,
    pub(super) attachment_ordinal: i64,
    pub(super) url: String,
}

#[derive(Debug, Clone, serde::Deserialize, skyzen::FromRow)]
pub(super) struct ContextDebugMessageToolCallRow {
    pub(super) session_id: String,
    pub(super) message_ordinal: i64,
    pub(super) tool_call_ordinal: i64,
    pub(super) name: String,
}

#[derive(Debug, Clone)]
pub struct ContextDebugSnapshot {
    pub session_id: String,
    pub timestamp: String,
    pub model: String,
    pub provider_id: String,
    pub prompt: String,
    pub prompt_attachments: Vec<String>,
    pub assembled_messages: Vec<ContextDebugSnapshotMessage>,
}

#[derive(Debug, Clone)]
pub struct ContextDebugSnapshotMessage {
    pub role: String,
    pub content: String,
    pub attachments: Vec<String>,
    pub tool_call_id: Option<String>,
    pub tool_call_names: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct NotificationCallbackConsumeResult {
    pub id: String,
    pub session_id: Option<String>,
    pub profile_id: Option<String>,
    pub callback_context_xml: Option<String>,
    pub callback_message: Option<String>,
    pub has_callback: bool,
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
pub(super) struct BoolValueRow {
    pub(super) value: bool,
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
    pub(super) checkpoint_blob: Vec<u8>,
    pub(super) last_message_db_id: Option<i64>,
    pub(super) updated_at: String,
}

/// Parameters for upserting a persisted session context snapshot.
pub struct SessionContextSnapshotUpsert {
    pub checkpoint: aither_agent::AgentCheckpoint,
    /// The DB `messages.id` of the last message at snapshot time.
    /// `None` means the snapshot was taken with an empty history.
    pub last_message_db_id: Option<i64>,
}

/// Structured snapshot loaded from SQL-backed session context storage.
#[derive(Debug, Clone)]
pub struct StoredSessionContextSnapshot {
    pub checkpoint: aither_agent::AgentCheckpoint,
    /// The DB `messages.id` of the last message at snapshot time.
    pub last_message_db_id: Option<i64>,
    pub updated_at: String,
}

/// Parameters for creating a scheduled task.
pub struct CreateScheduledTaskParams<'a> {
    pub id: &'a str,
    pub profile_id: &'a str,
    pub session_id: Option<&'a str>,
    pub name: &'a str,
    pub trigger: &'a crate::task_trigger::PersistedTaskTrigger,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ProfileEnvironmentKind {
    Local,
    Container,
}

impl ProfileEnvironmentKind {
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

pub(super) fn parse_answer_value_kind(
    kind: &str,
    selected_values: Vec<String>,
    custom_text: Option<String>,
) -> Result<AnswerValue, ChatDbError> {
    match kind {
        "selected" => Ok(AnswerValue::Selected(selected_values)),
        "custom" => Ok(AnswerValue::Custom(custom_text.unwrap_or_default())),
        "skipped" => Ok(AnswerValue::Skipped),
        other => Err(storage_error(format!(
            "invalid ask-user answer kind: {other}"
        ))),
    }
}

pub(super) fn normalize_profile_preferences(
    environment: ProfileEnvironmentKind,
    preferences: Option<&ProfilePreferences>,
) -> Result<Option<ProfilePreferences>, ChatDbError> {
    let mut preferences = preferences.cloned().unwrap_or_default();
    match environment {
        ProfileEnvironmentKind::Local => {
            preferences.container = None;
        }
        ProfileEnvironmentKind::Container => {}
    }
    Ok(preferences.container.is_some().then_some(preferences))
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
