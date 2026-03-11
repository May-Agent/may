//! Persisted session runtime state and server-owned pending messages.

/// Persisted runtime state for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimeState {
    Idle,
    Running,
    WaitingPermission,
    WaitingAskUser,
    WaitingTerminalInput,
    PendingImmediate,
    Queued,
}

impl SessionRuntimeState {
    /// Returns the stable string form used in the database.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingPermission => "waiting_permission",
            Self::WaitingAskUser => "waiting_ask_user",
            Self::WaitingTerminalInput => "waiting_terminal_input",
            Self::PendingImmediate => "pending_immediate",
            Self::Queued => "queued",
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
            "waiting_ask_user" => Ok(Self::WaitingAskUser),
            "waiting_terminal_input" => Ok(Self::WaitingTerminalInput),
            "pending_immediate" => Ok(Self::PendingImmediate),
            "queued" => Ok(Self::Queued),
            other => Err(format!("unknown session runtime state: {other}")),
        }
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
    pub hidden_context_xml: Option<String>,
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

/// Persisted ask-user request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserRequest {
    pub id: String,
    pub questions: Vec<PersistedAskUserQuestion>,
}

/// Persisted runtime summary used by session lists and runtime endpoints.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSummary {
    pub state: SessionRuntimeState,
    #[serde(default)]
    pub queued_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_immediate: Option<PendingUserMessage>,
    #[serde(default)]
    pub queued_messages: Vec<PendingUserMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_permission: Option<PersistedPermissionRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_ask_user: Option<PersistedAskUserRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Default for SessionRuntimeSummary {
    fn default() -> Self {
        Self {
            state: SessionRuntimeState::Idle,
            queued_count: 0,
            pending_immediate: None,
            queued_messages: Vec::new(),
            waiting_permission: None,
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

impl SessionRuntimeSummary {
    /// Recompute the authoritative state from queued/pending/waiting payloads.
    pub fn normalize_state(&mut self) {
        self.queued_count = self.queued_messages.len();
        self.state = if self.waiting_permission.is_some() {
            SessionRuntimeState::WaitingPermission
        } else if self.waiting_ask_user.is_some() {
            SessionRuntimeState::WaitingAskUser
        } else if self.pending_immediate.is_some() {
            SessionRuntimeState::PendingImmediate
        } else if !self.queued_messages.is_empty() {
            SessionRuntimeState::Queued
        } else if self.state == SessionRuntimeState::Running {
            SessionRuntimeState::Running
        } else {
            SessionRuntimeState::Idle
        };
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
    pub mode: String,
    pub script: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}
