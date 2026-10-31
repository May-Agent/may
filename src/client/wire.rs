//! Agent events as the daemon streams them to the TUI.
//!
//! The TUI renders the agent's own event stream; [`WireAgentEvent`] carries
//! the events it renders across the socket and turns back into
//! [`aither_agent::AgentEvent`] on the client, so the TUI's rendering code
//! is the same whether the agent runs in-process or in the daemon.

use aither_agent::{AgentError, AgentEvent, TimedEvent};
use aither_core::llm::{ToolResult, Usage};
use serde::{Deserialize, Serialize};

/// An agent failure, as it crosses the socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireAgentError {
    /// The model provider failed.
    Llm {
        /// Provider error.
        message: String,
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

impl From<&AgentError> for WireAgentError {
    fn from(error: &AgentError) -> Self {
        match error {
            AgentError::Llm(message) => Self::Llm {
                message: message.clone(),
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
            WireAgentError::Llm { message } => Self::Llm(message),
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
    /// The run ended.
    End {
        /// How it ended.
        outcome: RunEnd,
    },
}

/// One `session_runtime` event: the session's blocking-state snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeEvent {
    /// Session the snapshot belongs to.
    pub session_id: String,
    /// Its runtime summary.
    pub runtime: SessionRuntimeSnapshot,
}

/// The session's blocking state, as `GET /api/sessions/{id}/runtime` and the
/// `session_runtime` event report it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRuntimeSnapshot {
    /// Top-level runtime state.
    pub state: crate::session::SessionRuntimeState,
    /// Plan-mode state.
    pub plan_state: crate::session::SessionPlanState,
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionPollItem {
    /// Request id, the one `respond` answers.
    pub id: String,
    /// The request.
    #[serde(flatten)]
    pub request: crate::session::PersistedPermissionRequestPayload,
}

/// A workspace request blocking a session, as polls report it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
        let wire = WireAgentEvent::from_agent(Err(&AgentError::Llm("overloaded".to_string())))
            .expect("rendered error");
        let back = wire.into_timed(jiff::Timestamp::UNIX_EPOCH);
        assert!(matches!(back, Err(AgentError::Llm(message)) if message == "overloaded"));
    }
}
