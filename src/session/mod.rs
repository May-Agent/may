//! Session management for May agent.
//!
//! Each session has its own sandbox directory and chat history.

mod history;
mod manager;
mod store;

pub use history::{ChatHistory, ChatHistoryError, ChatMessage, MessageRole};
pub use manager::{Session, SessionError, SessionId, SessionManager, SessionSummary};
pub use store::{SessionMetadata, SessionStore};
