//! Agent events as the daemon streams them to the TUI.
//!
//! The TUI renders the agent's own event stream; [`WireAgentEvent`] carries
//! the events it renders across the socket and turns back into
//! [`aither_agent::AgentEvent`] on the client, so the TUI's rendering code
//! is the same whether the agent runs in-process or in the daemon.

use aither_agent::{AgentError, AgentEvent, TimedEvent};
use aither_core::llm::{Failure, FailureKind, ToolResult, Usage};
use serde::{Deserialize, Serialize};

/// An agent failure, as it crosses the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireAgentError {
    /// The model provider failed.
    Llm {
        /// Provider error.
        message: String,
        /// How the failure classifies.
        failure: WireFailure,
    },
    /// A tool failed.
    ToolExecution {
        /// Tool name.
        name: String,
        /// Failure.
        error: String,
    },
    /// The iteration limit was reached.
    MaxIterations {
        /// The limit.
        limit: usize,
    },
    /// A tool the model called does not exist.
    ToolNotFound {
        /// Tool name.
        name: String,
    },
    /// Anything else, as its message.
    Other {
        /// Display text of the error.
        message: String,
    },
}

/// A provider [`Failure`], as it crosses the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireFailure {
    /// What kind of failure it is.
    pub kind: WireFailureKind,
    /// The delay the upstream asked for, in milliseconds.
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}

/// [`FailureKind`], as it crosses the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFailureKind {
    /// No response came back from the network.
    Network,
    /// The upstream answered with a temporary fault.
    Transient,
    /// A quota or usage limit is exhausted.
    Quota,
    /// Anything a retry cannot fix.
    Permanent,
}

impl From<Failure> for WireFailure {
    fn from(failure: Failure) -> Self {
        Self {
            kind: match failure.kind {
                FailureKind::Network => WireFailureKind::Network,
                FailureKind::Transient => WireFailureKind::Transient,
                FailureKind::Quota => WireFailureKind::Quota,
                FailureKind::Permanent => WireFailureKind::Permanent,
            },
            retry_after_ms: failure
                .retry_after
                .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
        }
    }
}

impl From<WireFailure> for Failure {
    fn from(failure: WireFailure) -> Self {
        Self::new(match failure.kind {
            WireFailureKind::Network => FailureKind::Network,
            WireFailureKind::Transient => FailureKind::Transient,
            WireFailureKind::Quota => FailureKind::Quota,
            WireFailureKind::Permanent => FailureKind::Permanent,
        })
        .with_retry_after(failure.retry_after_ms.map(std::time::Duration::from_millis))
    }
}

impl From<&AgentError> for WireAgentError {
    fn from(error: &AgentError) -> Self {
        match error {
            AgentError::Llm { message, failure } => Self::Llm {
                message: message.clone(),
                failure: (*failure).into(),
            },
            AgentError::ToolExecution { name, error } => Self::ToolExecution {
                name: name.clone(),
                error: error.clone(),
            },
            AgentError::MaxIterations { limit } => Self::MaxIterations { limit: *limit },
            AgentError::ToolNotFound { name } => Self::ToolNotFound { name: name.clone() },
            other => Self::Other {
                message: other.to_string(),
            },
        }
    }
}

impl From<WireAgentError> for AgentError {
    fn from(error: WireAgentError) -> Self {
        match error {
            WireAgentError::Llm { message, failure } => Self::Llm {
                message,
                failure: failure.into(),
            },
            WireAgentError::ToolExecution { name, error } => Self::ToolExecution { name, error },
            WireAgentError::MaxIterations { limit } => Self::MaxIterations { limit },
            WireAgentError::ToolNotFound { name } => Self::ToolNotFound { name },
            WireAgentError::Other { message } => Self::Config(message),
        }
    }
}

/// The agent events a UI renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireAgentEvent {
    /// Assistant text chunk.
    Text {
        /// The chunk.
        text: String,
    },
    /// Reasoning chunk.
    Reasoning {
        /// The chunk.
        text: String,
    },
    /// Tool-call arguments streaming in.
    ToolCallDelta {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments fragment.
        arguments_fragment: String,
    },
    /// A tool call starts.
    ToolCallStart {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// JSON arguments.
        arguments: String,
    },
    /// A tool call finished.
    ToolCallEnd {
        /// Call id.
        id: String,
        /// Tool name.
        name: String,
        /// Its result.
        result: ToolResult,
    },
    /// A provider-hosted tool call started.
    BuiltInToolCall {
        /// Call id.
        id: String,
        /// Hosted tool name.
        tool: String,
        /// Query, URL or code.
        detail: String,
    },
    /// A provider-hosted tool call finished.
    BuiltInToolResult {
        /// Call id.
        id: String,
        /// Hosted tool name.
        tool: String,
        /// Result summary.
        result: String,
    },
    /// Token usage of a request.
    Usage {
        /// The usage.
        usage: Usage,
    },
    /// A model response ended; `has_tool_calls` is false when the next
    /// text, if any, is a separate reply rather than a continuation.
    TurnComplete {
        /// The turn number.
        turn: usize,
        /// Whether the response called tools.
        has_tool_calls: bool,
    },
    /// The agent finished the run.
    Complete {
        /// Full assistant text.
        final_text: String,
        /// Turns taken.
        turns: usize,
    },
    /// The agent failed.
    Error {
        /// The failure.
        error: WireAgentError,
    },
}

impl WireAgentEvent {
    /// The wire form of `event`, or `None` for events no UI renders.
    #[must_use]
    pub fn from_agent(event: Result<&AgentEvent, &AgentError>) -> Option<Self> {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                return Some(Self::Error {
                    error: error.into(),
                });
            }
        };
        Some(match event {
            AgentEvent::Text(text) => Self::Text { text: text.clone() },
            AgentEvent::Reasoning(text) => Self::Reasoning { text: text.clone() },
            AgentEvent::ToolCallDelta {
                id,
                name,
                arguments_fragment,
            } => Self::ToolCallDelta {
                id: id.clone(),
                name: name.clone(),
                arguments_fragment: arguments_fragment.clone(),
            },
            AgentEvent::ToolCallStart {
                id,
                name,
                arguments,
            } => Self::ToolCallStart {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            },
            AgentEvent::ToolCallEnd { id, name, result } => Self::ToolCallEnd {
                id: id.clone(),
                name: name.clone(),
                result: result.clone(),
            },
            AgentEvent::BuiltInToolCall { id, tool, detail } => Self::BuiltInToolCall {
                id: id.clone(),
                tool: tool.clone(),
                detail: detail.clone(),
            },
            AgentEvent::BuiltInToolResult { id, tool, result } => Self::BuiltInToolResult {
                id: id.clone(),
                tool: tool.clone(),
                result: result.clone(),
            },
            AgentEvent::Usage(usage) => Self::Usage {
                usage: usage.clone(),
            },
            AgentEvent::TurnComplete {
                turn,
                has_tool_calls,
            } => Self::TurnComplete {
                turn: *turn,
                has_tool_calls: *has_tool_calls,
            },
            AgentEvent::Complete { final_text, turns } => Self::Complete {
                final_text: final_text.clone(),
                turns: *turns,
            },
            AgentEvent::Error(error) => Self::Error {
                error: error.into(),
            },
            _ => return None,
        })
    }

    /// The agent event this wire event stands for, stamped `at`.
    ///
    /// # Errors
    /// An `error` event becomes the agent error it reports.
    pub fn into_timed(self, at: jiff::Timestamp) -> Result<TimedEvent, AgentError> {
        let event = match self {
            Self::Text { text } => AgentEvent::Text(text),
            Self::Reasoning { text } => AgentEvent::Reasoning(text),
            Self::ToolCallDelta {
                id,
                name,
                arguments_fragment,
            } => AgentEvent::ToolCallDelta {
                id,
                name,
                arguments_fragment,
            },
            Self::ToolCallStart {
                id,
                name,
                arguments,
            } => AgentEvent::ToolCallStart {
                id,
                name,
                arguments,
            },
            Self::ToolCallEnd { id, name, result } => AgentEvent::ToolCallEnd { id, name, result },
            Self::BuiltInToolCall { id, tool, detail } => {
                AgentEvent::BuiltInToolCall { id, tool, detail }
            }
            Self::BuiltInToolResult { id, tool, result } => {
                AgentEvent::BuiltInToolResult { id, tool, result }
            }
            Self::Usage { usage } => AgentEvent::Usage(usage),
            Self::TurnComplete {
                turn,
                has_tool_calls,
            } => AgentEvent::TurnComplete {
                turn,
                has_tool_calls,
            },
            Self::Complete { final_text, turns } => AgentEvent::Complete { final_text, turns },
            Self::Error { error } => return Err(error.into()),
        };
        Ok(TimedEvent { at, event })
    }
}

/// How a daemon run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEnd {
    /// The turn completed.
    Completed,
    /// The turn paused for a browser takeover.
    Paused,
    /// The turn was stopped.
    Cancelled,
    /// The turn failed.
    Failed,
}

/// One frame of the `session_agent` event kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAgentFrame {
    /// Session the run belongs to.
    pub session_id: String,
    /// Turn the frame belongs to.
    pub turn_id: String,
    /// When the daemon emitted the frame.
    pub at: jiff::Timestamp,
    /// What happened.
    pub body: SessionAgentBody,
}

/// One frame of the `session_suggestion` event kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionFrame {
    /// The aux request that produced the update.
    #[serde(default)]
    pub request_id: u64,
    /// Session the suggestion belongs to.
    #[serde(default)]
    pub session_id: String,
    /// `ready` with `text`, or `cleared`.
    pub state: String,
    /// The suggested prompt when `state` is `ready`.
    #[serde(default)]
    pub text: Option<String>,
}

/// One frame of the `session_recap` event kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecapFrame {
    /// The aux request that produced the update.
    #[serde(default)]
    pub request_id: u64,
    /// Session the recap belongs to.
    #[serde(default)]
    pub session_id: String,
    /// `ready` with a card, or `error`.
    pub state: String,
    /// The catch-up summary when `state` is `ready`.
    #[serde(default)]
    pub summary: Option<String>,
    /// The suggested next step, when the recap named one.
    #[serde(default)]
    pub next_action: Option<String>,
}

/// What a parked run waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum HoldReason {
    /// An upstream quota reset.
    Quota {
        /// When the run continues.
        #[schema(value_type = String)]
        until: jiff::Timestamp,
    },
    /// The backoff before a retry of a transient provider failure.
    Retry {
        /// When the run continues.
        #[schema(value_type = String)]
        until: jiff::Timestamp,
        /// Which retry on the provider this is.
        retry: u32,
    },
    /// The network is gone; the run continues once it is back.
    Network,
}

/// What a `session_agent` frame reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionAgentBody {
    /// A run started.
    Start {
        /// The user message the run answers, when it has one.
        #[serde(default)]
        content: Option<String>,
        /// Its row id in the session history, when the run stored it.
        #[serde(default)]
        message_id: Option<i32>,
    },
    /// An agent event of the run.
    Agent {
        /// The event.
        event: WireAgentEvent,
    },
    /// A reasoning phase of the run closed; `seconds` is how long the model
    /// spent on it, whether or not the provider returned readable text.
    ReasoningEnd {
        /// Seconds the phase lasted. Sub-second phases keep the fraction.
        seconds: f64,
    },
    /// The run is parked.
    Hold {
        /// What the run waits for.
        #[serde(flatten)]
        reason: HoldReason,
        /// The upstream message behind the wait.
        message: String,
    },
    /// The text the run streamed so far is discarded: its attempt failed and
    /// the prompt is resubmitted.
    Retract,
    /// The run ended.
    End {
        /// How it ended.
        outcome: RunEnd,
    },
}

/// One `session_runtime` event: the session's blocking-state snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeEvent {
    /// Session the snapshot belongs to.
    pub session_id: String,
    /// Its runtime summary.
    pub runtime: SessionRuntimeSnapshot,
}

/// One `session_index` event: coverage of the worktree index in repo-search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexSnapshot {
    /// Session the coverage belongs to; empty when the subscriber named a
    /// cwd and no session is persisted yet.
    pub session_id: String,
    /// The worktree index's coverage; `None` when repo-search has not
    /// indexed it yet.
    pub index: Option<IndexCoverage>,
    /// Why coverage could not be read; set when repo-search is unreachable.
    /// Absent on a successful poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One `session_issues` event: the issues the session's tasks link, as the
/// issue store reports them now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIssuesFrame {
    /// Session the issues are linked from.
    pub session_id: String,
    /// One brief per linked issue.
    pub issues: Vec<crate::tui::sidebar::IssueBrief>,
}

/// How much of a worktree index carries a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexCoverage {
    /// The index's phase.
    pub phase: repo_search_client::protocol::WirePhase,
    /// Chunks with a vector.
    pub with_vector: u64,
    /// Chunks in total.
    pub total: u64,
}

/// The session's blocking state, as `GET /api/sessions/{id}/runtime` and the
/// `session_runtime` event report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSnapshot {
    /// Top-level runtime state.
    pub state: crate::session::SessionRuntimeState,
    /// Plan-mode state.
    pub plan_state: crate::session::SessionPlanState,
    /// Yolo switch: every approval request is granted at once.
    #[serde(default)]
    pub yolo: bool,
    /// Interactive-resource sub-state (browser, terminal).
    pub interactions: SessionInteractionSnapshot,
    /// Messages waiting to be processed.
    pub queued_count: usize,
    /// A message that must be dispatched before queued work.
    #[serde(default)]
    pub pending_immediate: Option<crate::session::PendingUserMessage>,
    /// Messages queued for later dispatch.
    #[serde(default)]
    pub queued_messages: Vec<crate::session::PendingUserMessage>,
    /// Permission request blocking the session, if any.
    #[serde(default)]
    pub waiting_permission: Option<PermissionPollItem>,
    /// Workspace request blocking the session, if any.
    #[serde(default)]
    pub waiting_workspace: Option<WorkspacePollItem>,
    /// Ask-user request blocking the session, if any.
    #[serde(default)]
    pub waiting_ask_user: Option<crate::session::PersistedAskUserRequest>,
    /// Questions the agent asked that are neither answered nor withdrawn,
    /// oldest first.
    #[serde(default)]
    pub open_questions: Vec<crate::session::PersistedAskUserRequest>,
}

impl Default for SessionRuntimeSnapshot {
    fn default() -> Self {
        Self {
            state: crate::session::SessionRuntimeState::Idle,
            plan_state: crate::session::SessionPlanState::Disabled,
            yolo: false,
            interactions: SessionInteractionSnapshot::default(),
            queued_count: 0,
            pending_immediate: None,
            queued_messages: Vec::new(),
            waiting_permission: None,
            waiting_workspace: None,
            waiting_ask_user: None,
            open_questions: Vec::new(),
        }
    }
}

/// Interactive-resource sub-state of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInteractionSnapshot {
    /// Browser interaction sub-state.
    #[serde(default)]
    pub browser: BrowserInteractionSnapshot,
    /// Terminal runtime sub-state.
    #[serde(default)]
    pub terminal: TerminalInteractionSnapshot,
}

/// Whether the browser is under user control.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractionSnapshot {
    /// The user holds the browser while the agent waits.
    #[serde(default)]
    pub takeover_active: bool,
}

/// The session's terminal, as the daemon reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalInteractionSnapshot {
    /// Task currently attached, if any.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Terminal output file, if any.
    #[serde(default)]
    pub output_file: Option<String>,
    /// Preview of its recent output, if any.
    #[serde(default)]
    pub output_preview: Option<String>,
    /// Notice to show the user about pending input, if any.
    #[serde(default)]
    pub input_notice: Option<String>,
}

/// A permission request blocking a session, as polls report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionPollItem {
    /// Request id, the one `respond` answers.
    pub id: String,
    /// The request.
    #[serde(flatten)]
    pub request: crate::session::PersistedPermissionRequestPayload,
}

/// A workspace request blocking a session, as polls report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePollItem {
    /// Request id, the one `respond` answers.
    pub id: String,
    /// Path access is requested for.
    pub path: String,
    /// Why it is needed.
    pub reason: String,
    /// Session that asked, when it is not the connected one.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `POST /api/sessions` request body.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    /// Session id the client pre-generated; the daemon assigns one otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Model to bind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Profile to create it under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// Host directory the session works in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Repository to bind: `owner/repo`, a local path, or none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

/// `PUT /api/sessions/{id}` request body.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSessionRequest {
    /// New title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning-effort override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Host directory the session works in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Repository to bind: `owner/repo`, a local path, or none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// One-shot/non-interactive flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub non_interactive: Option<bool>,
}

/// `POST /api/sessions/{id}/rewind` request body.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindRequest {
    /// Index into the session's stored message list where the cut starts.
    pub index: usize,
}

/// One `GET /api/sessions/{id}` response, the fields a client session needs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WireSession {
    /// Session id.
    pub id: String,
    /// Title.
    pub title: String,
    /// Last-modified timestamp, RFC 3339.
    pub last_modified: String,
    /// Profile the session belongs to.
    pub profile_id: Option<String>,
    /// Model bound to the session.
    pub model: String,
    /// Reasoning effort override, if any.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Repository binding, if any.
    #[serde(default)]
    pub repo: Option<String>,
}

/// One session PR over the wire, mirroring the daemon's `ApiPr`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WirePr {
    /// Per-repository PR number.
    pub number: i64,
    /// PR title.
    pub title: String,
    /// Lifecycle state.
    pub state: String,
    /// `may` or `github`.
    pub backend: String,
    /// External PR number, when the backend created one.
    #[serde(default)]
    pub external_number: Option<i64>,
    /// Associated issue number, when linked.
    #[serde(default)]
    pub issue_number: Option<i64>,
    /// Whether the PR merges itself once checks pass.
    #[serde(default)]
    pub auto_merge: bool,
    /// Latest update timestamp.
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use aither_agent::{AgentError, AgentEvent};
    use aither_core::llm::ToolResult;

    use super::WireAgentEvent;

    #[test]
    fn tool_results_survive_the_round_trip() {
        let event = AgentEvent::ToolCallEnd {
            id: "t1".to_string(),
            name: "terminal".to_string(),
            result: ToolResult::text("done"),
        };
        let wire = WireAgentEvent::from_agent(Ok(&event)).expect("rendered event");
        let json = serde_json::to_string(&wire).expect("serialize");
        let back: WireAgentEvent = serde_json::from_str(&json).expect("deserialize");
        let timed = back.into_timed(jiff::Timestamp::UNIX_EPOCH).expect("event");
        assert!(matches!(
            timed.event,
            AgentEvent::ToolCallEnd { ref id, ref result, .. } if id == "t1" && *result == ToolResult::text("done")
        ));
    }

    #[test]
    fn errors_come_back_as_errors() {
        let failure = aither_core::llm::Failure::transient()
            .with_retry_after(Some(std::time::Duration::from_secs(5)));
        let wire = WireAgentEvent::from_agent(Err(&AgentError::Llm {
            message: "overloaded".to_string(),
            failure,
        }))
        .expect("rendered error");
        let back = wire.into_timed(jiff::Timestamp::UNIX_EPOCH);
        assert!(matches!(
            back,
            Err(AgentError::Llm { message, failure: back_failure })
                if message == "overloaded" && back_failure == failure
        ));
    }
}
