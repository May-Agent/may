//! Persisted session runtime state and server-owned pending messages.

use super::SessionPlanState;
use crate::PermissionRequestMode;
use crate::xml_fragment::XmlFragment;

/// Persisted runtime state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimeState {
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
}

impl Default for SessionRuntimeState {
    fn default() -> Self {
        Self::Idle
    }
}

impl core::str::FromStr for SessionRuntimeState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "waiting_permission" => Ok(Self::WaitingPermission),
            "waiting_workspace" => Ok(Self::WaitingWorkspace),
            "waiting_ask_user" => Ok(Self::WaitingAskUser),
            "waiting_terminal_input" => Ok(Self::WaitingTerminalInput),
            other => {
                let mut message = String::with_capacity(31 + other.len());
                message.push_str("unknown session runtime state: ");
                message.push_str(other);
                Err(message)
            }
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
}

/// Persisted browser interaction state owned by the server runtime.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractionState {
    #[serde(default)]
    pub takeover_active: bool,
    #[serde(default)]
    pub takeover_alert_pending: bool,
}

/// Persisted interactive-resource state for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionState {
    #[serde(default)]
    pub browser: BrowserInteractionState,
    #[serde(default)]
    pub terminal: TerminalRuntimeState,
}

impl SessionInteractionState {
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.browser.takeover_active
    }
}

/// Server-owned user message waiting to be processed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingUserMessage {
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<PersistedAttachment>,
    #[serde(default)]
    pub hidden_context_xml: Option<XmlFragment>,
    pub created_at: String,
    pub delivery_mode: PendingUserMessageMode,
}

/// Delivery mode for a pending user message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingUserMessageMode {
    Steer,
    Queue,
}

impl Default for PendingUserMessageMode {
    fn default() -> Self {
        Self::Steer
    }
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

impl core::str::FromStr for PendingUserMessageMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "steer" => Ok(Self::Steer),
            "queue" => Ok(Self::Queue),
            other => {
                let mut message = String::with_capacity(35 + other.len());
                message.push_str("unknown pending user message mode: ");
                message.push_str(other);
                Err(message)
            }
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
    pub state: SessionRuntimeState,
    #[serde(default)]
    pub plan_state: SessionPlanState,
    #[serde(default)]
    pub interactions: SessionInteractionState,
    #[serde(default)]
    pub queued_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_immediate: Option<PendingUserMessage>,
    #[serde(default)]
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
    pub fn has_pending_messages(&self) -> bool {
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAttachment {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub attachment_type: String,
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
