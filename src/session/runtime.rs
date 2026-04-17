//! Persisted session runtime state and server-owned pending messages.

use super::SessionPlanState;
use crate::PermissionRequestMode;
use crate::xml::XmlBlock;

const fn is_false(value: &bool) -> bool {
    !*value
}

const fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

const fn session_plan_state_is_disabled(value: &SessionPlanState) -> bool {
    matches!(value, SessionPlanState::Disabled)
}

const fn session_runtime_state_is_idle(value: &SessionRuntimeState) -> bool {
    matches!(value, SessionRuntimeState::Idle)
}

/// Persisted runtime state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SessionRuntimeState {
    #[default]
    Idle,
    Running,
    WaitingPermission,
    WaitingWorkspace,
    WaitingAskUser,
    WaitingTerminalInput,
}

impl SessionRuntimeState {
    /// Returns the stable string form used in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingPermission => "waiting_permission",
            Self::WaitingWorkspace => "waiting_workspace",
            Self::WaitingAskUser => "waiting_ask_user",
            Self::WaitingTerminalInput => "waiting_terminal_input",
        }
    }

    /// Returns true when this state should prevent auto-archive.
    #[must_use]
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }

    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown session runtime state: {value}")]
pub struct SessionRuntimeStateParseError {
    value: String,
}

impl core::str::FromStr for SessionRuntimeState {
    type Err = SessionRuntimeStateParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "waiting_permission" => Ok(Self::WaitingPermission),
            "waiting_workspace" => Ok(Self::WaitingWorkspace),
            "waiting_ask_user" => Ok(Self::WaitingAskUser),
            "waiting_terminal_input" => Ok(Self::WaitingTerminalInput),
            other => Err(SessionRuntimeStateParseError {
                value: other.to_string(),
            }),
        }
    }
}

/// Persisted terminal-runtime metadata owned by the server runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRuntimeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_notice: Option<String>,
    /// Why the command was promoted to the background (populated when the
    /// runtime state reflects a live terminal task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_reason: Option<aither_sandbox::BackgroundReason>,
}

impl TerminalRuntimeState {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.task_id.is_none()
            && self.output_file.is_none()
            && self.output_preview.is_none()
            && self.input_notice.is_none()
            && self.background_reason.is_none()
    }

    #[must_use]
    pub fn with_input_notice(mut self, task_id: Option<String>, notice: String) -> Self {
        if let Some(task_id) = task_id {
            self.task_id = Some(task_id);
        }
        self.input_notice = Some(notice);
        self
    }
}

/// Persisted browser interaction state owned by the server runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractionState {
    #[serde(default, skip_serializing_if = "is_false")]
    pub takeover_active: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub takeover_alert_pending: bool,
}

impl BrowserInteractionState {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.takeover_active && !self.takeover_alert_pending
    }
}

/// Persisted interactive-resource state for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionState {
    #[serde(default, skip_serializing_if = "BrowserInteractionState::is_empty")]
    pub browser: BrowserInteractionState,
    #[serde(default, skip_serializing_if = "TerminalRuntimeState::is_empty")]
    pub terminal: TerminalRuntimeState,
}

impl SessionInteractionState {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.browser.takeover_active
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.browser.is_empty() && self.terminal.is_empty()
    }
}

/// Server-owned user message waiting to be processed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUserMessage {
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<PersistedAttachment>,
    /// Structured hidden context injected as a system message for the next run.
    /// This is intentionally markup-friendly text, not strict XML.
    #[serde(default)]
    pub hidden_context_xml: Option<XmlBlock>,
    pub created_at: String,
    pub delivery_mode: PendingUserMessageMode,
}

/// Delivery mode for a pending user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PendingUserMessageMode {
    #[default]
    Steer,
    Queue,
}

impl PendingUserMessageMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown pending user message mode: {value}")]
pub struct PendingUserMessageModeParseError {
    value: String,
}

impl core::str::FromStr for PendingUserMessageMode {
    type Err = PendingUserMessageModeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "steer" => Ok(Self::Steer),
            "queue" => Ok(Self::Queue),
            other => Err(PendingUserMessageModeParseError {
                value: other.to_string(),
            }),
        }
    }
}

/// Persisted ask-user request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserRequest {
    pub id: String,
    pub questions: Vec<PersistedAskUserQuestion>,
}

/// Persisted workspace request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedWorkspaceRequest {
    pub id: String,
    pub request: PersistedWorkspaceRequestPayload,
}

/// Persisted runtime summary used by session lists and runtime endpoints.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSummary {
    #[serde(default, skip_serializing_if = "session_runtime_state_is_idle")]
    pub state: SessionRuntimeState,
    #[serde(default, skip_serializing_if = "session_plan_state_is_disabled")]
    pub plan_state: SessionPlanState,
    #[serde(default, skip_serializing_if = "SessionInteractionState::is_empty")]
    pub interactions: SessionInteractionState,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub queued_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_immediate: Option<PendingUserMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<PendingUserMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_permission: Option<PersistedPermissionRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_workspace: Option<PersistedWorkspaceRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_ask_user: Option<PersistedAskUserRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Default for SessionRuntimeSummary {
    fn default() -> Self {
        Self {
            state: SessionRuntimeState::Idle,
            plan_state: SessionPlanState::Disabled,
            interactions: SessionInteractionState::default(),
            queued_count: 0,
            pending_immediate: None,
            queued_messages: Vec::new(),
            waiting_permission: None,
            waiting_workspace: None,
            waiting_ask_user: None,
            updated_at: None,
        }
    }
}

/// Persisted permission request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedPermissionRequest {
    pub id: String,
    pub request: PersistedPermissionRequestPayload,
}

/// Persisted workspace request payload visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedWorkspaceRequestPayload {
    pub path: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl SessionRuntimeSummary {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.state.is_idle()
            && !self.plan_state.is_enabled()
            && self.interactions.is_empty()
            && self.queued_count == 0
            && self.pending_immediate.is_none()
            && self.queued_messages.is_empty()
            && self.waiting_permission.is_none()
            && self.waiting_workspace.is_none()
            && self.waiting_ask_user.is_none()
    }

    /// Keep overflow steer messages ahead of regular queued work.
    pub fn push_overflow_steer(&mut self, message: PendingUserMessage) {
        self.queued_messages.insert(0, message);
    }

    /// Recompute derived runtime fields from persisted payloads.
    pub fn normalize_state(&mut self) {
        self.queued_count =
            self.queued_messages.len() + usize::from(self.pending_immediate.is_some());
        self.state = if self.waiting_permission.is_some() {
            SessionRuntimeState::WaitingPermission
        } else if self.waiting_workspace.is_some() {
            SessionRuntimeState::WaitingWorkspace
        } else if self.waiting_ask_user.is_some() {
            SessionRuntimeState::WaitingAskUser
        } else if matches!(
            self.state,
            SessionRuntimeState::WaitingPermission
                | SessionRuntimeState::WaitingWorkspace
                | SessionRuntimeState::WaitingAskUser
        ) {
            SessionRuntimeState::Idle
        } else if self.state == SessionRuntimeState::WaitingTerminalInput {
            SessionRuntimeState::WaitingTerminalInput
        } else if self.state == SessionRuntimeState::Running {
            SessionRuntimeState::Running
        } else {
            SessionRuntimeState::Idle
        };
    }

    #[must_use]
    pub const fn has_pending_immediate(&self) -> bool {
        self.pending_immediate.is_some()
    }

    #[must_use]
    pub const fn has_pending_messages(&self) -> bool {
        self.pending_immediate.is_some() || !self.queued_messages.is_empty()
    }

    #[must_use]
    pub fn can_dispatch_follow_up(&self) -> bool {
        self.state == SessionRuntimeState::Idle
            && !self.interactions.is_active()
            && self.has_pending_messages()
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state.is_active()
            || self.has_pending_messages()
            || self.plan_state.blocks_auto_archive()
            || self.interactions.is_active()
    }
}

/// Persisted attachment shape for pending user messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    File,
    Text,
    Document,
    Pdf,
    Audio,
    Video,
}

impl AttachmentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
            Self::Text => "text",
            Self::Document => "document",
            Self::Pdf => "pdf",
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

impl std::fmt::Display for AttachmentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAttachment {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub attachment_type: AttachmentKind,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_data_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_content: Option<String>,
}

/// Persisted ask-user question shape for refresh-safe UI recovery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserQuestion {
    pub section: String,
    pub question: String,
    pub option: Vec<String>,
    pub multi_select: bool,
}

/// Persisted permission request payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedPermissionRequestPayload {
    pub mode: PermissionRequestMode,
    pub script: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}
