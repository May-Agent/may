//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

pub(crate) mod history;
pub mod layout;
mod manager;
mod plan;
pub mod runtime;
mod store;

pub use history::{
    ChatHistory, ChatHistoryError, ChatMessage, ChatMessageStats, MessageRole, SessionMetadata,
};
pub use layout::{MayHomePaths, ProfileHomePaths, SessionPaths};
pub use manager::{
    Session, SessionError, SessionId, SessionManager, SessionSummary, list_summaries_from_db,
};
pub use plan::SessionPlanState;
pub use runtime::{
    PendingUserMessage, PendingUserMessageMode, PersistedAskUserQuestion, PersistedAskUserRequest,
    PersistedAttachment, PersistedPermissionRequest, PersistedPermissionRequestPayload,
    SessionRuntimeState, SessionRuntimeSummary,
};
pub use store::SessionStore;
