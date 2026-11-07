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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SessionRuntimeState {
    /// No run or pending interaction.
    #[default]
    Idle,
    /// An agent turn is in flight.
    Running,
    /// Blocked on a permission request.
    WaitingPermission,
    /// Blocked on a workspace approval request.
    WaitingWorkspace,
    /// Blocked on an ask-user question.
    WaitingAskUser,
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
        }
    }

    /// Whether a run or a blocking interaction is in flight.
    #[must_use]
    pub const fn is_executing(self) -> bool {
        !matches!(self, Self::Idle)
    }

    /// Whether the state is `Idle`.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

/// A persisted session runtime state string did not match a known variant.
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
            other => Err(SessionRuntimeStateParseError {
                value: other.to_string(),
            }),
        }
    }
}

/// Persisted terminal-runtime metadata owned by the server runtime.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRuntimeState {
    /// Id of the live terminal task, when one is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Path of the captured terminal output file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,
    /// Tail preview of the terminal output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    /// Notice shown to the user about pending terminal input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_notice: Option<String>,
}

impl TerminalRuntimeState {
    /// Whether no terminal state is recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.task_id.is_none()
            && self.output_file.is_none()
            && self.output_preview.is_none()
            && self.input_notice.is_none()
    }

    /// Return a copy with `input_notice` (and `task_id`, when provided) set.
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
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractionState {
    /// A user peer currently owns browser input.
    #[serde(default, skip_serializing_if = "is_false")]
    pub takeover_active: bool,
    /// A takeover-ended alert still needs to surface.
    #[serde(default, skip_serializing_if = "is_false")]
    pub takeover_alert_pending: bool,
}

impl BrowserInteractionState {
    /// Whether no browser interaction state is recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.takeover_active && !self.takeover_alert_pending
    }
}

/// Persisted interactive-resource state for a session.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionState {
    /// Browser interaction sub-state.
    #[serde(default, skip_serializing_if = "BrowserInteractionState::is_empty")]
    pub browser: BrowserInteractionState,
    /// Terminal runtime sub-state.
    #[serde(default, skip_serializing_if = "TerminalRuntimeState::is_empty")]
    pub terminal: TerminalRuntimeState,
}

impl SessionInteractionState {
    /// Whether any interaction currently blocks the agent (browser takeover).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.browser.takeover_active
    }

    /// Whether no interaction state is recorded.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.browser.is_empty() && self.terminal.is_empty()
    }
}

/// Lifecycle state of a queued user message: `pending` (claimable),
/// `claimed` (a run owns it), `committed` (landed in the transcript),
/// `failed` (ended with a recorded error).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PendingMessageState {
    /// Waiting to run.
    #[default]
    Pending,
    /// A run claimed it and is preparing or running it.
    Claimed,
    /// Appended to the transcript by its claiming run.
    Committed,
    /// The run ended on a non-retryable error; `error` explains.
    Failed,
}

impl PendingMessageState {
    /// Stored form.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Committed => "committed",
            Self::Failed => "failed",
        }
    }
}

impl std::str::FromStr for PendingMessageState {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "committed" => Ok(Self::Committed),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("invalid pending message state: {value}")),
        }
    }
}

/// Server-owned user message waiting to be processed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PendingUserMessage {
    /// Queue row id once stored; `None` before insertion.
    #[serde(default)]
    pub id: Option<i64>,
    /// Lifecycle state.
    #[serde(default)]
    pub state: PendingMessageState,
    /// Recorded error when `state` is `failed`.
    #[serde(default)]
    pub error: Option<String>,
    /// Message body text.
    pub content: String,
    /// Attachments carried by the message.
    #[serde(default)]
    pub attachments: Vec<PersistedAttachment>,
    /// Structured hidden context injected as a system message for the next run.
    /// This is intentionally markup-friendly text, not strict XML.
    #[serde(default)]
    pub hidden_context_xml: Option<XmlBlock>,
    /// Creation timestamp as an ISO string.
    pub created_at: String,
    /// How the message should be delivered (steer vs queue).
    pub delivery_mode: PendingUserMessageMode,
}

/// Delivery mode for a pending user message.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PendingUserMessageMode {
    /// Interrupt the current turn immediately.
    #[default]
    Steer,
    /// Run after the current turn completes.
    Queue,
}

impl PendingUserMessageMode {
    /// Persisted `snake_case` string form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queue",
        }
    }
}

/// A persisted delivery-mode string did not match a known variant.
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserRequest {
    /// Request id.
    pub id: String,
    /// Questions presented to the user.
    pub questions: Vec<PersistedAskUserQuestion>,
}

/// Persisted workspace request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedWorkspaceRequest {
    /// Request id.
    pub id: String,
    /// Workspace request payload.
    pub request: PersistedWorkspaceRequestPayload,
}

/// Persisted runtime summary used by session lists and runtime endpoints.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSummary {
    /// Top-level runtime state.
    #[serde(default, skip_serializing_if = "session_runtime_state_is_idle")]
    pub state: SessionRuntimeState,
    /// Plan-mode state.
    #[serde(default, skip_serializing_if = "session_plan_state_is_disabled")]
    pub plan_state: SessionPlanState,
    /// Yolo switch: every approval request of the session is granted at once.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub yolo: bool,
    /// Interactive-resource sub-state (browser, terminal).
    #[serde(default, skip_serializing_if = "SessionInteractionState::is_empty")]
    pub interactions: SessionInteractionState,
    /// Number of messages waiting to be processed.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub queued_count: usize,
    /// A message that must be dispatched before queued work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_immediate: Option<PendingUserMessage>,
    /// Messages queued for later dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<PendingUserMessage>,
    /// Permission request blocking the session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_permission: Option<PersistedPermissionRequest>,
    /// Workspace request blocking the session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_workspace: Option<PersistedWorkspaceRequest>,
    /// Ask-user request blocking the session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_ask_user: Option<PersistedAskUserRequest>,
    /// Questions the agent asked that are neither answered nor withdrawn,
    /// oldest first. No run waits on them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<PersistedAskUserRequest>,
    /// Last update timestamp as an ISO string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Default for SessionRuntimeSummary {
    fn default() -> Self {
        Self {
            state: SessionRuntimeState::Idle,
            plan_state: SessionPlanState::Disabled,
            yolo: false,
            interactions: SessionInteractionState::default(),
            queued_count: 0,
            pending_immediate: None,
            queued_messages: Vec::new(),
            waiting_permission: None,
            waiting_workspace: None,
            waiting_ask_user: None,
            open_questions: Vec::new(),
            updated_at: None,
        }
    }
}

/// Persisted permission request visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedPermissionRequest {
    /// Request id.
    pub id: String,
    /// Permission request payload.
    pub request: PersistedPermissionRequestPayload,
}

/// Persisted workspace request payload visible after refresh.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedWorkspaceRequestPayload {
    /// Directory being requested.
    pub path: String,
    /// Why access is needed.
    pub reason: String,
    /// Session the request belongs to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl SessionRuntimeSummary {
    /// Whether the summary carries no runtime information.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.state.is_idle()
            && !self.plan_state.is_enabled()
            && !self.yolo
            && self.interactions.is_empty()
            && self.queued_count == 0
            && self.pending_immediate.is_none()
            && self.queued_messages.is_empty()
            && self.waiting_permission.is_none()
            && self.waiting_workspace.is_none()
            && self.waiting_ask_user.is_none()
            && self.open_questions.is_empty()
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
        } else if self.state == SessionRuntimeState::Running {
            SessionRuntimeState::Running
        } else {
            SessionRuntimeState::Idle
        };
    }

    /// Whether an immediate-dispatch message is pending.
    #[must_use]
    pub const fn has_pending_immediate(&self) -> bool {
        self.pending_immediate.is_some()
    }

    /// Whether any user message is pending (immediate or queued).
    #[must_use]
    pub fn has_pending_messages(&self) -> bool {
        self.pending_immediate
            .as_ref()
            .is_some_and(|m| m.state == PendingMessageState::Pending)
            || self
                .queued_messages
                .iter()
                .any(|m| m.state == PendingMessageState::Pending)
    }

    /// Whether a queued message can be dispatched now (idle, no active interaction, pending messages).
    #[must_use]
    pub fn can_dispatch_follow_up(&self) -> bool {
        self.state == SessionRuntimeState::Idle
            && !self.interactions.is_active()
            && self.has_pending_messages()
    }

    /// Whether the session should block auto-archive: running, pending messages, active plan, or live interaction.
    #[must_use]
    pub fn blocks_auto_archive(&self) -> bool {
        self.state.is_executing()
            || self.has_pending_messages()
            || self.plan_state.blocks_auto_archive()
            || self.interactions.is_active()
    }

    /// Whether the session admits a new request: not running, no pending
    /// messages and no live interaction. A draft plan does not block
    /// admission — it only keeps the session out of auto-archive.
    #[must_use]
    pub fn admits_new_request(&self) -> bool {
        !(self.state.is_executing() || self.has_pending_messages() || self.interactions.is_active())
    }
}

#[cfg(test)]
mod tests {
    use super::{SessionPlanState, SessionRuntimeState, SessionRuntimeSummary};

    #[test]
    fn an_idle_draft_session_admits_a_request_but_blocks_auto_archive() {
        let summary = SessionRuntimeSummary {
            plan_state: SessionPlanState::Draft,
            ..SessionRuntimeSummary::default()
        };
        assert!(summary.admits_new_request());
        assert!(summary.blocks_auto_archive());
    }

    #[test]
    fn a_running_session_admits_no_request_and_blocks_auto_archive() {
        let summary = SessionRuntimeSummary {
            state: SessionRuntimeState::Running,
            ..SessionRuntimeSummary::default()
        };
        assert!(!summary.admits_new_request());
        assert!(summary.blocks_auto_archive());
    }
}

/// Persisted attachment shape for pending user messages.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// Image attachment.
    Image,
    /// Generic file attachment.
    File,
    /// Inline text attachment.
    Text,
    /// Document attachment.
    Document,
    /// PDF attachment.
    Pdf,
    /// Audio attachment.
    Audio,
    /// Video attachment.
    Video,
}

impl AttachmentKind {
    /// Return the persisted name of the attachment kind.
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

/// Attachment record persisted with a pending user message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAttachment {
    /// Attachment ID.
    pub id: String,
    /// Display filename.
    pub name: String,
    /// Attachment category.
    #[serde(rename = "type")]
    pub attachment_type: AttachmentKind,
    /// Content URL or local path.
    pub url: String,
    /// Inline data URL for uploaded content, when stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_data_url: Option<String>,
    /// Extracted text content for text-like attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_content: Option<String>,
}

/// Persisted ask-user question shape for refresh-safe UI recovery.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedAskUserQuestion {
    /// Section the question is grouped under.
    pub section: String,
    /// Question text presented to the user.
    pub question: String,
    /// Selectable options for the question.
    pub option: Vec<String>,
    /// Whether multiple options may be chosen.
    pub multi_select: bool,
    /// Set when the question asks for a secret value instead of a choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<PersistedSecretRequest>,
}

/// The secret an `ask_user` question collects: its name and who may read it.
/// The value is never part of the question or its answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedSecretRequest {
    /// Environment variable name the secret is stored and injected under.
    pub name: String,
    /// Whether the secret is global or held by the asking session.
    pub scope: crate::secrets::SecretScope,
}

/// Persisted permission request payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersistedPermissionRequestPayload {
    /// Permission mode being requested.
    pub mode: PermissionRequestMode,
    /// Script or command the permission covers.
    pub script: String,
    /// Session the request belongs to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Agent that raised the request: the session's own agent or one of its
    /// subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// How May reads the script, for the approval prompt; absent for a
    /// request that covers no script.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reading: Option<crate::command_reading::CommandReading>,
}

impl PersistedPermissionRequestPayload {
    /// A request for `script`, read the way the approval prompt shows it.
    #[must_use]
    pub fn new(
        mode: PermissionRequestMode,
        script: String,
        session_id: Option<String>,
        agent_id: Option<String>,
    ) -> Self {
        let reading = (mode != PermissionRequestMode::Publish)
            .then(|| crate::command_reading::CommandReading::of(&script));
        Self {
            mode,
            script,
            session_id,
            agent_id,
            reading,
        }
    }
}
