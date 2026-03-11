//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

pub(crate) mod history;
mod manager;
pub mod runtime;
mod store;

pub use history::{
    ChatHistory, ChatHistoryError, ChatMessage, ChatMessageStats, MessageRole, SessionMetadata,
};
pub use manager::{
    Session, SessionError, SessionId, SessionManager, SessionSummary, list_summaries_from_db,
};
pub use runtime::{
    PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion, PersistedAskUserRequest,
    PersistedAttachment, PersistedPermissionRequest, PersistedPermissionRequestPayload,
    SessionRuntimeState, SessionRuntimeSummary,
};
pub use store::SessionStore;
