//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

mod context_snapshot;
pub(crate) mod history;
pub mod layout;
mod manager;
pub(crate) mod persisted_message;
mod plan;
pub mod runtime;
mod runtime_service;
mod store;

pub use context_snapshot::{
    PersistContextSnapshotError, PersistedContextSnapshot, checkpoint_from_hook,
    history_delta_since_snapshot, llm_row_ids, load_persisted_context_snapshot,
    parse_persisted_context_snapshot, persist_agent_context_snapshot, persist_checkpoint_from_hook,
};
pub use history::{
    ChatHistory, ChatMessage, ChatMessageStats, HistoryReplayError, MessageRole, SessionMetadata,
    ToolEventRecord,
};
pub use layout::{MayHomePaths, ProfileHomePaths, SessionPaths};
pub use manager::{
    RewindPoint, Session, SessionError, SessionId, SessionManager, SessionSummary,
    initialize_session_persistence, list_summaries_from_db, load_history_from_db,
    reconcile_container_profile_sessions, summarize_db_sessions,
};
pub(crate) use persisted_message::{PersistedMessage, PersistedMessageError};
pub use plan::SessionPlanState;
pub use runtime::{
    AttachmentKind, BrowserInteractionState, PendingUserMessage, PendingUserMessageMode,
    PersistedAskUserQuestion, PersistedAskUserRequest, PersistedAttachment,
    PersistedPermissionRequest, PersistedPermissionRequestPayload, PersistedWorkspaceRequest,
    PersistedWorkspaceRequestPayload, SessionInteractionState, SessionRuntimeState,
    SessionRuntimeSummary, TerminalRuntimeState,
};
pub use runtime_service::SessionRuntimeService;
pub use store::{SandboxResourceLayout, SessionStore, StoreError};
