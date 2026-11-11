//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

mod binding;
mod context_snapshot;
mod directory;
pub(crate) mod history;
pub mod import;
pub mod layout;
mod manager;
pub mod open_questions;
#[cfg(test)]
mod open_questions_tests;
pub(crate) mod persisted_message;
mod plan;
pub(crate) mod runs;
#[cfg(test)]
mod runs_tests;
pub mod runtime;
mod runtime_service;
mod store;
pub(crate) mod tool_result_wire;
mod user_wait;

pub use binding::{BindingError, RepoBinding, group_sessions, project_label};
pub use context_snapshot::{
    PersistContextSnapshotError, PersistedContextSnapshot, checkpoint_watermark,
    load_persisted_context_snapshot, parse_persisted_context_snapshot,
    persist_agent_context_snapshot, persist_run_checkpoint,
};
pub use directory::SessionDirectory;
pub use history::{
    ChatHistory, ChatMessage, ChatMessageStats, HistoryReplayError, MessageRole,
    MessageTranslation, SessionMetadata, ToolEventRecord, chat_messages_from_transcript,
    language_display_name,
};
pub use layout::{MayHomePaths, ProfileHomePaths, SessionPaths};
pub use manager::{
    RewindPoint, Session, SessionError, SessionId, SessionLaunch, SessionManager, SessionSummary,
    initialize_session_persistence, list_summaries_from_db, load_history_from_db,
    reconcile_container_profile_sessions, summarize_db_sessions,
};
pub(crate) use persisted_message::{PersistedMessage, PersistedMessageError};
pub use plan::SessionPlanState;
pub use runs::{
    AcquireError, CancelReason, RunOutcome, RunReleased, SessionBusy, SessionRunGuard, SessionRuns,
};
pub use runtime::{
    AttachmentKind, BrowserInteractionState, PendingMessageState, PendingUserMessage,
    PendingUserMessageMode, PersistedAskUserQuestion, PersistedAskUserRequest, PersistedAttachment,
    PersistedPermissionRequest, PersistedPermissionRequestPayload, PersistedSecretRequest,
    PersistedWorkspaceRequest, PersistedWorkspaceRequestPayload, SessionInteractionState,
    SessionRuntimeState, SessionRuntimeSummary, TerminalRuntimeState,
};
pub use runtime_service::{AnswerError, AskUserAnswer, ResolvedRequest, SessionRuntimeService};
pub use store::{SandboxResourceLayout, SessionStore, StoreError};
pub use user_wait::{UserAsk, UserWait};
