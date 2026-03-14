//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

pub(crate) mod history;
mod context_snapshot;
pub mod layout;
mod manager;
mod plan;
pub mod runtime;
mod runtime_service;
mod store;

pub use context_snapshot::{
    ContextSnapshotRecord, PersistedContextSnapshot, StoredContextWindowMetadata,
    history_delta_since_snapshot, load_persisted_context_snapshot,
    parse_persisted_context_snapshot, persist_agent_context_snapshot,
    persist_context_snapshot_record, snapshot_record_from_agent,
};
pub use history::{
    ChatHistory, ChatHistoryError, ChatMessage, ChatMessageStats, MessageRole, SessionMetadata,
};
pub use layout::{MayHomePaths, ProfileHomePaths, SessionPaths};
pub use manager::{
    Session, SessionError, SessionId, SessionManager, SessionSummary, list_summaries_from_db,
};
pub use plan::SessionPlanState;
pub use runtime::{
    BrowserInteractionState, PendingUserMessage, PendingUserMessageMode,
    PersistedAskUserQuestion, PersistedAskUserRequest, PersistedAttachment,
    PersistedPermissionRequest, PersistedPermissionRequestPayload, SessionInteractionState,
    SessionRuntimeState, SessionRuntimeSummary,
};
pub use runtime_service::SessionRuntimeService;
pub use store::SessionStore;
