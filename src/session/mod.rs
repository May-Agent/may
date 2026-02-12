//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

pub(crate) mod history;
mod manager;
mod store;

pub use history::{ChatHistory, ChatHistoryError, ChatMessage, MessageRole, SessionMetadata};
pub use manager::{
    Session, SessionError, SessionId, SessionManager, SessionSummary, list_summaries_from_db,
};
pub use store::SessionStore;
